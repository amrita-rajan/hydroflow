use crate::{people, Encodable, Message, Opts, CONTACTS_ADDR, DIAGNOSES_ADDR};

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use hydroflow::{
    compiled::{pull::SymmetricHashJoin, ForEach, Pivot},
    scheduled::{
        ctx::{RecvCtx, SendCtx},
        handoff::VecHandoff,
        Hydroflow,
    },
    tl, tlt,
};
use rand::Rng;
use tokio::{net::TcpListener, runtime::Runtime};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

pub(crate) fn run_database(opts: Opts) {
    let rt = Runtime::new().unwrap();

    let all_people = people::get_people();

    let mut df = Hydroflow::new();

    let (contacts_in, contacts_out) = df.add_channel_input();
    let (diagnoses_in, diagnoses_out) = df.add_channel_input();
    let (people_in, people_out) = df.add_channel_input();

    let stream = rt
        .block_on(TcpListener::bind(format!("localhost:{}", opts.port)))
        .unwrap();
    println!("awaiting connection: {:?}", stream);
    let (stream, _) = rt.block_on(stream.accept()).unwrap();
    println!("connected");
    let (reader, writer) = stream.into_split();

    std::thread::spawn(move || {
        let mut t = 0;
        let mut rng = rand::thread_rng();
        for (id, (name, phone)) in all_people.clone() {
            people_in.give(Some((id.to_owned(), (name.to_owned(), phone.to_owned()))));
        }
        loop {
            t += 1;
            match rng.gen_range(0..2) as usize {
                0 => {
                    // New contact.
                    if all_people.len() >= 2 {
                        let p1 = rng.gen_range(0..all_people.len());
                        let p2 = rng.gen_range(0..all_people.len());
                        if p1 != p2 {
                            contacts_in.give(Some((all_people[p1].0, all_people[p2].0, t)));
                            contacts_in.flush();
                        }
                    }
                }
                1 => {
                    // Diagnosis.
                    if !all_people.is_empty() {
                        let p = rng.gen_range(0..all_people.len());
                        diagnoses_in.give(Some((all_people[p].0, (t, t + 14))));
                        diagnoses_in.flush();
                    }
                }
                _ => unreachable!(),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });

    let mut writer = FramedWrite::new(writer, LengthDelimitedCodec::new());
    let send = df.add_sink(move |recv: &RecvCtx<VecHandoff<Message>>| {
        for v in recv.take_inner() {
            // TODO(justin): Pretty bad to keep allocating like this. Need a
            // better interface.
            let mut buf = Vec::new();
            v.encode(&mut buf);
            rt.block_on(writer.send(buf.into())).unwrap();
        }
    });

    let (mut ins, mut out) = df.add_n_in_m_out(
        2,
        1,
        |recvs: &[&RecvCtx<VecHandoff<Message>>], sends: &[&SendCtx<VecHandoff<_>>]| {
            for recv in recvs {
                sends[0].give(recv.take_inner());
            }
        },
    );

    df.add_edge(out.pop().unwrap(), send);

    let (contacts_merge, diagnoses_merge) = (ins.pop().unwrap(), ins.pop().unwrap());

    let (encode_contacts_in, encode_contacts_out) = df.add_inout(
        |recv: &RecvCtx<VecHandoff<(&'static str, &'static str, usize)>>, send| {
            for p in recv.take_inner() {
                let mut v = Vec::new();
                p.encode(&mut v);
                send.give(Some(Message::Data {
                    address: CONTACTS_ADDR,
                    data: v,
                }));
            }
        },
    );

    df.add_edge(contacts_out, encode_contacts_in);
    df.add_edge(encode_contacts_out, contacts_merge);

    let (encode_diagnoses_in, encode_diagnoses_out) = df.add_inout(
        |recv: &RecvCtx<VecHandoff<(&'static str, (usize, usize))>>, send| {
            for p in recv.take_inner() {
                let mut v = Vec::new();
                p.encode(&mut v);
                send.give(Some(Message::Data {
                    address: DIAGNOSES_ADDR,
                    data: v,
                }));
            }
        },
    );

    df.add_edge(diagnoses_out, encode_diagnoses_in);
    df.add_edge(encode_diagnoses_out, diagnoses_merge);

    let reader = FramedRead::new(reader, LengthDelimitedCodec::new());

    let notifs = df.add_input_from_stream::<_, VecHandoff<_>, _>(
        reader.map(|buf| Some(<(String, usize)>::decode(&buf.unwrap().to_vec()))),
    );

    type SubgraphIn = tlt!(
        VecHandoff::<(String, usize)>,
        VecHandoff::<(String, (String, String))>,
    );

    let mut join_state = Default::default();
    let (tl!(notif_sink, people_sink), tl!()) =
        df.add_subgraph::<_, SubgraphIn, ()>(move |tl!(notifs, people), tl!()| {
            let join = SymmetricHashJoin::new(
                notifs.take_inner().into_iter(),
                people.take_inner().into_iter(),
                &mut join_state,
            )
            .map(|(_id, t, (name, phone))| (name, phone, t));
            let notify =
                ForEach::new(|(name, phone, t)| println!("notifying {}, {}@{}", name, phone, t));
            let pivot = Pivot::new(join, notify);
            pivot.run();
        });

    df.add_edge(notifs, notif_sink);
    df.add_edge(people_out, people_sink);

    df.run().unwrap();
}