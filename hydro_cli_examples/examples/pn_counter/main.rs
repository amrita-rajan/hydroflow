use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::rc::Rc;
use std::time::Duration;

use hydroflow::hydroflow_syntax;
use hydroflow::serde::{Deserialize, Serialize};
use hydroflow::tokio;
use hydroflow::util::cli::ConnectedDemux;
use hydroflow::util::cli::{ConnectedBidi, ConnectedSink, ConnectedSource};
use hydroflow::util::{deserialize_from_bytes, serialize_to_bytes};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct IncrementRequest {
    tweet_id: u64,
    likes: i32,
}

type NextStateType = (u64, Rc<RefCell<(Vec<u32>, Vec<u32>)>>);

#[derive(Serialize, Deserialize, Clone, Debug)]
enum GossipOrIncrement {
    Gossip(Vec<NextStateType>),
    Increment(u64, i32),
}

#[tokio::main]
async fn main() {
    let mut ports = hydroflow::util::cli::init().await;

    let my_id: Vec<usize> = serde_json::from_str(&std::env::args().nth(1).unwrap()).unwrap();
    let my_id = my_id[0];
    let num_replicas: Vec<usize> = serde_json::from_str(&std::env::args().nth(2).unwrap()).unwrap();
    let num_replicas = num_replicas[0];

    let increment_requests = ports
        .remove("increment_requests")
        .unwrap()
        .connect::<ConnectedBidi>()
        .await
        .into_source();

    let query_responses = ports
        .remove("query_responses")
        .unwrap()
        .connect::<ConnectedBidi>()
        .await
        .into_sink();

    let to_peer = ports
        .remove("to_peer")
        .unwrap()
        .connect::<ConnectedDemux<ConnectedBidi>>()
        .await
        .into_sink();

    let from_peer = ports
        .remove("from_peer")
        .unwrap()
        .connect::<ConnectedBidi>()
        .await
        .into_source();

    let f1 = async move {
        #[cfg(target_os = "linux")]
        loop {
            let x = procinfo::pid::stat_self().unwrap();
            let bytes = x.rss * 1024 * 4;
            println!("memory,{}", bytes);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };

    let df = hydroflow_syntax! {
        next_state = merge()
            -> fold::<'static>((HashMap::<u64, Rc<RefCell<(Vec<u32>, Vec<u32>)>>>::new(), HashSet::new(), 0), |(mut cur_state, mut modified_tweets, last_tick), goi| {
                if context.current_tick() != last_tick {
                    modified_tweets.clear();
                }

                match goi {
                    GossipOrIncrement::Gossip(gossip) => {
                        for (counter_id, gossip_rc) in gossip.iter() {
                            let gossip_borrowed = gossip_rc.as_ref().borrow();
                            let (pos, neg) = gossip_borrowed.deref();
                            let cur_value = cur_state.entry(*counter_id).or_insert(Rc::new(RefCell::new((
                                vec![0; num_replicas], vec![0; num_replicas]
                            ))));
                            let mut cur_value = cur_value.as_ref().borrow_mut();

                            for i in 0..num_replicas {
                                if pos[i] > cur_value.0[i] {
                                    cur_value.0[i] = pos[i];
                                    modified_tweets.insert(*counter_id);
                                }

                                if neg[i] > cur_value.1[i] {
                                    cur_value.1[i] = neg[i];
                                    modified_tweets.insert(*counter_id);
                                }
                            }
                        }
                    }
                    GossipOrIncrement::Increment(counter_id, delta) => {
                        let cur_value = cur_state.entry(counter_id).or_insert(Rc::new(RefCell::new((
                            vec![0; num_replicas], vec![0; num_replicas]
                        ))));
                        let mut cur_value = cur_value.as_ref().borrow_mut();

                        if delta > 0 {
                            cur_value.0[my_id] += delta as u32;
                        } else {
                            cur_value.1[my_id] += (-delta) as u32;
                        }

                        modified_tweets.insert(counter_id);
                    }
                }

                (cur_state, modified_tweets, context.current_tick())
            })
            -> filter(|(_, _, tick)| *tick == context.current_tick())
            -> filter(|(_, modified_tweets, _)| !modified_tweets.is_empty())
            -> map(|(state, modified_tweets, _)| modified_tweets.iter().map(|t| (*t, state.get(t).unwrap().clone())).collect::<Vec<_>>())
            -> tee();

        source_stream(from_peer)
            -> map(|x| deserialize_from_bytes::<GossipOrIncrement>(&x.unwrap()).unwrap())
            -> next_state;

        source_stream(increment_requests)
            -> map(|x| deserialize_from_bytes::<IncrementRequest>(&x.unwrap()).unwrap())
            -> map(|t| GossipOrIncrement::Increment(t.tweet_id, t.likes))
            -> next_state;

        all_peers = source_iter(0..num_replicas)
            -> filter(|x| *x != my_id);

        all_peers -> [0] broadcaster;
        next_state -> [1] broadcaster;
        broadcaster = cross_join::<'static, 'tick>()
            -> map(|(peer, state)| {
                (peer as u32, serialize_to_bytes(GossipOrIncrement::Gossip(state)))
            })
            -> dest_sink(to_peer);

        next_state
            -> flat_map(|a: Vec<NextStateType>| {
                a.into_iter().map(|(k, rc_array)| {
                    let rc_borrowed = rc_array.as_ref().borrow();
                    let (pos, neg) = rc_borrowed.deref();
                    (k, pos.iter().sum::<u32>() as i32 - neg.iter().sum::<u32>() as i32)
                }).collect::<Vec<_>>()
            })
            -> map(serialize_to_bytes::<(u64, i32)>)
            -> dest_sink(query_responses);
    };

    // initial memory
    #[cfg(target_os = "linux")]
    {
        let x = procinfo::pid::stat_self().unwrap();
        let bytes = x.rss * 1024 * 4;
        println!("memory,{}", bytes);
    }

    let f1_handle = tokio::spawn(f1);
    hydroflow::util::cli::launch_flow(df).await;
    f1_handle.abort();
}
