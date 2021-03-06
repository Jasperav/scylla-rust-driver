/// ConnectionKeeper keeps a Connection to some address and works to keep it open
use crate::routing::ShardInfo;
use crate::transport::errors::QueryError;
use crate::transport::{
    connection,
    connection::{Connection, ConnectionConfig, VerifiedKeyspaceName},
};

use futures::{future::RemoteHandle, FutureExt};
use std::net::SocketAddr;
use std::sync::Arc;

/// ConnectionKeeper keeps a Connection to some address and works to keep it open
pub struct ConnectionKeeper {
    conn_state_receiver: tokio::sync::watch::Receiver<ConnectionState>,
    _worker_handle: RemoteHandle<()>,
}

#[derive(Clone)]
pub enum ConnectionState {
    Initializing, // First connect attempt ongoing
    Connected(Arc<Connection>),
    Broken(QueryError),
}

/// Works in the background to keep the connection open
struct ConnectionKeeperWorker {
    address: SocketAddr,
    config: ConnectionConfig,
    shard_info: Option<ShardInfo>,

    shard_info_sender: Option<ShardInfoSender>,
    conn_state_sender: tokio::sync::watch::Sender<ConnectionState>,

    // Keyspace send in "USE <keyspace name>" when opening each connection
    used_keyspace: Option<VerifiedKeyspaceName>,
}

pub type ShardInfoSender = Arc<std::sync::Mutex<tokio::sync::watch::Sender<Option<ShardInfo>>>>;

impl ConnectionKeeper {
    /// Creates new ConnectionKeeper that starts a connection in the background
    /// # Arguments
    ///
    /// * `address` - IP address to connect to
    /// * `compression` - preferred compression method to use
    /// * `shard_info` - ShardInfo to use, will connect to shard number `shard_info.shard`
    /// * `shard_info_sender` - channel to send new ShardInfo after each connection creation
    pub fn new(
        address: SocketAddr,
        config: ConnectionConfig,
        shard_info: Option<ShardInfo>,
        shard_info_sender: Option<ShardInfoSender>,
        keyspace_name: Option<VerifiedKeyspaceName>,
    ) -> Self {
        let (conn_state_sender, conn_state_receiver) =
            tokio::sync::watch::channel(ConnectionState::Initializing);

        let worker = ConnectionKeeperWorker {
            address,
            config,
            shard_info,
            shard_info_sender,
            conn_state_sender,
            used_keyspace: keyspace_name,
        };

        let (fut, worker_handle) = worker.work().remote_handle();
        tokio::spawn(fut);

        ConnectionKeeper {
            conn_state_receiver,
            _worker_handle: worker_handle,
        }
    }

    /// Get current connection state, returns immediately
    pub fn connection_state(&self) -> ConnectionState {
        self.conn_state_receiver.borrow().clone()
    }

    pub async fn wait_until_initialized(&self) {
        match &*self.conn_state_receiver.borrow() {
            ConnectionState::Initializing => {}
            _ => return,
        };

        let mut my_receiver = self.conn_state_receiver.clone();

        my_receiver
            .changed()
            .await
            .expect("Bug in ConnectionKeeper::wait_until_initialized");
        // Worker can't stop while we have &self to struct with worker_handle

        // Now state must be != Initializing
        debug_assert!(!matches!(
            &*self.conn_state_receiver.borrow(),
            ConnectionState::Initializing
        ));
    }

    /// Wait for the connection to initialize and get it if succesfylly connected
    pub async fn get_connection(&self) -> Result<Arc<Connection>, QueryError> {
        self.wait_until_initialized().await;

        match self.connection_state() {
            ConnectionState::Connected(conn) => Ok(conn),
            ConnectionState::Broken(e) => Err(e),
            _ => unreachable!(),
        }
    }

    pub async fn use_keyspace(
        &self,
        keyspace_name: &VerifiedKeyspaceName,
    ) -> Result<(), QueryError> {
        // ConnectionKeeper doesn't have reconnecting yet so this will be ok for now
        // TODO: Modify once ConnectionKeeper gets reconnecting

        self.get_connection()
            .await?
            .use_keyspace(keyspace_name)
            .await
    }
}

impl ConnectionKeeperWorker {
    pub async fn work(self) {
        let cur_connection = self.open_new_connection().await;

        match &cur_connection {
            Ok(conn) => {
                let _ = self
                    .conn_state_sender
                    .send(ConnectionState::Connected(conn.clone()));

                let new_shard_info: Option<ShardInfo> = conn.get_shard_info().clone();

                if let Some(sender) = &self.shard_info_sender {
                    // Ignore sending error
                    // If no one wants to get shard_info that's OK
                    // If lock is poisoned do nothing
                    if let Ok(sender_locked) = sender.lock() {
                        let _ = sender_locked.send(new_shard_info);
                    }
                }
            }
            Err(e) => {
                let _ = self
                    .conn_state_sender
                    .send(ConnectionState::Broken(e.clone()));
            } // TODO: Wait for connection to fail, then create new, loop it
        };
    }

    async fn open_new_connection(&self) -> Result<Arc<Connection>, QueryError> {
        let mut source_port: Option<u16> = None;
        if let Some(info) = &self.shard_info {
            source_port = Some(info.draw_source_port_for_shard(info.shard.into()));
        }

        let new_conn =
            connection::open_connection(self.address, source_port, self.config.clone()).await?;

        if let Some(keyspace_name) = &self.used_keyspace {
            let _ = new_conn.use_keyspace(&keyspace_name).await;
            // Ignore the error, used_keyspace could be set a long time ago and then deleted
            // user gets all errors from session.use_keyspace()
        }

        Ok(Arc::new(new_conn))
    }
}
