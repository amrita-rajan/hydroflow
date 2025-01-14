use std::{borrow::Cow, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use async_channel::{Receiver, Sender};
use async_ssh2_lite::{AsyncChannel, AsyncSession};
use async_trait::async_trait;
use futures::{AsyncWriteExt, StreamExt};
use hydroflow_cli_integration::ServerBindConfig;
use nanoid::nanoid;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::RwLock,
};

use crate::core::util::async_retry;

use super::{
    localhost::create_broadcast, LaunchedBinary, LaunchedHost, ResourceResult, ServerStrategy,
};

struct LaunchedSSHBinary {
    _resource_result: Arc<ResourceResult>,
    session: Option<AsyncSession<TcpStream>>,
    channel: AsyncChannel<TcpStream>,
    stdin_sender: Sender<String>,
    stdout_receivers: Arc<RwLock<Vec<Sender<String>>>>,
    stderr_receivers: Arc<RwLock<Vec<Sender<String>>>>,
}

#[async_trait]
impl LaunchedBinary for LaunchedSSHBinary {
    async fn stdin(&self) -> Sender<String> {
        self.stdin_sender.clone()
    }

    async fn stdout(&self) -> Receiver<String> {
        let mut receivers = self.stdout_receivers.write().await;
        let (sender, receiver) = async_channel::unbounded::<String>();
        receivers.push(sender);
        receiver
    }

    async fn stderr(&self) -> Receiver<String> {
        let mut receivers = self.stderr_receivers.write().await;
        let (sender, receiver) = async_channel::unbounded::<String>();
        receivers.push(sender);
        receiver
    }

    async fn exit_code(&self) -> Option<i32> {
        // until the program exits, the exit status is meaningless
        if self.channel.eof() {
            self.channel.exit_status().ok()
        } else {
            None
        }
    }

    async fn wait(&mut self) -> Option<i32> {
        self.channel.wait_eof().await.unwrap();
        let ret = self.exit_code().await;
        self.channel.wait_close().await.unwrap();
        ret
    }
}

impl Drop for LaunchedSSHBinary {
    fn drop(&mut self) {
        let session = self.session.take().unwrap();
        std::thread::scope(|s| {
            s.spawn(|| {
                let runtime = tokio::runtime::Builder::new_multi_thread().build().unwrap();
                runtime
                    .block_on(session.disconnect(None, "", None))
                    .unwrap();
            })
            .join()
            .unwrap();
        });
    }
}

#[async_trait]
pub trait LaunchedSSHHost: Send + Sync {
    fn server_config(&self, bind_type: &ServerStrategy) -> ServerBindConfig;
    fn resource_result(&self) -> &Arc<ResourceResult>;
    fn ssh_user(&self) -> &str;
    async fn open_ssh_session(&self) -> Result<AsyncSession<TcpStream>>;
}

#[async_trait]
impl<T: LaunchedSSHHost> LaunchedHost for T {
    fn server_config(&self, bind_type: &ServerStrategy) -> ServerBindConfig {
        LaunchedSSHHost::server_config(self, bind_type)
    }

    async fn launch_binary(
        &self,
        binary: Arc<(String, Vec<u8>)>,
        args: &[String],
    ) -> Result<Arc<RwLock<dyn LaunchedBinary>>> {
        let session = self.open_ssh_session().await?;

        let sftp = async_retry(
            &|| async { Ok(session.sftp().await?) },
            10,
            Duration::from_secs(1),
        )
        .await?;

        // we may be deploying multiple binaries, so give each a unique name
        let unique_name = &binary.0;

        let user = self.ssh_user();
        let binary_path = PathBuf::from(format!("/home/{user}/hydro-{unique_name}"));

        if sftp.stat(&binary_path).await.is_err() {
            let random = nanoid!(8);
            let temp_path = PathBuf::from(format!("/home/{user}/hydro-{random}"));
            let mut created_file = sftp.create(&temp_path).await?;
            println!("[hydro] uploading binary to /home/{user}/hydro-{random}");
            created_file.write_all(&binary.1).await?;

            let mut orig_file_stat = sftp.stat(&temp_path).await?;
            orig_file_stat.perm = Some(0o755); // allow the copied binary to be executed by anyone
            created_file.setstat(orig_file_stat).await?;
            created_file.close().await?;
            drop(created_file);
            println!("[hydro] uploaded binary to /home/{user}/hydro-{random}");

            sftp.rename(&temp_path, &binary_path, None).await?;
            println!("[hydro] moved binary to /home/{user}/hydro-{unique_name}");
        }
        drop(sftp);

        println!("[hydro] launching binary /home/{user}/hydro-{unique_name}");
        let mut channel = async_retry(
            &|| async {
                Ok(
                    tokio::time::timeout(Duration::from_secs(60), session.channel_session())
                        .await??,
                )
            },
            10,
            Duration::from_secs(1),
        )
        .await?;
        let binary_path_string = binary_path.to_str().unwrap();
        let args_string = args
            .iter()
            .map(|s| shell_escape::unix::escape(Cow::from(s)))
            .fold("".to_string(), |acc, v| format!("{acc} {v}"));
        channel
            .exec(&format!("{binary_path_string}{args_string}"))
            .await?;
        println!("[hydro] launched binary /home/{user}/hydro-{unique_name}");

        let (stdin_sender, mut stdin_receiver) = async_channel::unbounded::<String>();
        let mut stdin = channel.stream(0); // stream 0 is stdout/stdin, we use it for stdin
        tokio::spawn(async move {
            while let Some(line) = stdin_receiver.next().await {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let stdout_receivers = create_broadcast(channel.stream(0), |s| println!("{s}"));
        let stderr_receivers = create_broadcast(channel.stderr(), |s| eprintln!("{s}"));

        Ok(Arc::new(RwLock::new(LaunchedSSHBinary {
            _resource_result: self.resource_result().clone(),
            session: Some(session),
            channel,
            stdin_sender,
            stdout_receivers,
            stderr_receivers,
        })))
    }

    async fn forward_port(&self, addr: &SocketAddr) -> Result<SocketAddr> {
        let session = self.open_ssh_session().await?;

        let local_port = TcpListener::bind("127.0.0.1:0").await?;
        let local_addr = local_port.local_addr()?;

        let internal_ip = addr.ip().to_string();
        let port = addr.port();

        tokio::spawn(async move {
            #[allow(clippy::never_loop)]
            while let Ok((mut local_stream, _)) = local_port.accept().await {
                let mut channel = session
                    .channel_direct_tcpip(&internal_ip, port, None)
                    .await
                    .unwrap();
                let _ = tokio::io::copy_bidirectional(&mut local_stream, &mut channel).await;
                break;
                // TODO(shadaj): we should be returning an Arc so that we know
                // if anyone wants to connect to this forwarded port
            }

            println!("[hydro] closing forwarded port");
        });

        Ok(local_addr)
    }
}
