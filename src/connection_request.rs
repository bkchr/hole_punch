use context::ConnectionId;
use error::*;
use protocol::Protocol;
use stream::{NewStreamFuture, StreamHandle};
use timeout::Timeout;

use std::net::SocketAddr;
use std::time::Duration;

use tokio_core::reactor::Handle;

use futures::future::Join;
use futures::sync::oneshot;
use futures::{Future, Poll, Sink, Stream as FStream};

use serde::{Deserialize, Serialize};

use state_machine_future::RentToOwn;

#[derive(StateMachineFuture)]
pub enum ConnectionRequest<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[state_machine_future(start, transitions(WaitForRelayRequest))]
    WaitForSlaveAddressInfo {
        connection_id: ConnectionId,
        master: StreamHandle<P>,
        slave: StreamHandle<P>,
        master_address_info: Vec<SocketAddr>,
        slave_address_info_recv: oneshot::Receiver<Vec<SocketAddr>>,
        master_relay_con_recv: oneshot::Receiver<()>,
        timeout: Timeout,
        handle: Handle,
    },
    #[state_machine_future(transitions(WaitForNewStreams, Finished))]
    WaitForRelayRequest {
        connection_id: ConnectionId,
        master: StreamHandle<P>,
        slave: StreamHandle<P>,
        master_relay_con_recv: oneshot::Receiver<()>,
        timeout: Timeout,
        handle: Handle,
    },
    #[state_machine_future(transitions(Finished))]
    WaitForNewStreams {
        connection_id: ConnectionId,
        new_streams: Join<NewStreamFuture<P>, NewStreamFuture<P>>,
        timeout: Timeout,
        handle: Handle,
    },
    #[state_machine_future(ready)]
    Finished(()),
    #[state_machine_future(error)]
    ConnectionError(Error),
}

impl<P> ConnectionRequest<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(
        connection_id: ConnectionId,
        handle: &Handle,
        master: StreamHandle<P>,
        mut slave: StreamHandle<P>,
        master_address_info: Vec<SocketAddr>,
    ) -> (ConnectionRequestMasterHandle, ConnectionRequestSlaveHandle) {
        let (slave_send, slave_address_info_recv) = oneshot::channel();
        let (master_send, master_recv) = oneshot::channel();

        slave.send_msg(Protocol::RequestPrivateAdressInformation);

        let req = ConnectionRequest::start(
            connection_id,
            master,
            slave,
            master_address_info,
            slave_address_info_recv,
            master_recv,
            Timeout::new(Duration::from_secs(120), handle),
            handle.clone(),
        );
        handle.spawn(req.map_err(|e| error!("{:?}", e)));

        (
            ConnectionRequestMasterHandle::new(master_send),
            ConnectionRequestSlaveHandle::new(slave_send),
        )
    }
}

impl<P> PollConnectionRequest<P> for ConnectionRequest<P>
where
    P: 'static + Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn poll_wait_for_slave_address_info<'a>(
        state: &'a mut RentToOwn<'a, WaitForSlaveAddressInfo<P>>,
    ) -> Poll<AfterWaitForSlaveAddressInfo<P>, Error> {
        if state.timeout.poll().is_err() {
            bail!("timeout while waiting for slave address info.");
        }

        let address_info = try_ready!(state.slave_address_info_recv.poll());

        let mut state = state.take();
        state.slave.send_msg(Protocol::Connect(
            state.master_address_info,
            0,
            state.connection_id,
        ));
        state
            .master
            .send_msg(Protocol::Connect(address_info, 0, state.connection_id));

        transition!(WaitForRelayRequest {
            master: state.master,
            slave: state.slave,
            connection_id: state.connection_id,
            master_relay_con_recv: state.master_relay_con_recv,
            timeout: state.timeout,
            handle: state.handle,
        })
    }

    fn poll_wait_for_relay_request<'a>(
        state: &'a mut RentToOwn<'a, WaitForRelayRequest<P>>,
    ) -> Poll<AfterWaitForRelayRequest<P>, Error> {
        if state.timeout.poll().is_err() {
            transition!(Finished(()));
        }

        try_ready!(state.master_relay_con_recv.poll());

        let mut state = state.take();

        let new_streams = state.master.new_stream().join(state.slave.new_stream());
        transition!(WaitForNewStreams {
            connection_id: state.connection_id,
            new_streams,
            timeout: state.timeout,
            handle: state.handle,
        })
    }

    fn poll_wait_for_new_streams<'a>(
        state: &'a mut RentToOwn<'a, WaitForNewStreams<P>>,
    ) -> Poll<AfterWaitForNewStreams, Error> {
        if state.timeout.poll().is_err() {
            bail!("timeout while waiting for new streams");
        }

        let (mut stream0, mut stream1) = try_ready!(state.new_streams.poll());

        stream0.direct_send(Protocol::RelayConnection(state.connection_id))?;
        stream1.direct_send(Protocol::RelayConnection(state.connection_id))?;
        println!("RELAY");

        let (sink0, fstream0) = stream0.split();
        let (sink1, fstream1) = stream1.split();

        state
            .handle
            .spawn(sink0.send_all(fstream1).map_err(|_| ()).map(|_| ()));
        state
            .handle
            .spawn(sink1.send_all(fstream0).map_err(|_| ()).map(|_| ()));

        transition!(Finished(()))
    }
}

pub struct ConnectionRequestSlaveHandle {
    send: oneshot::Sender<Vec<SocketAddr>>,
}

impl ConnectionRequestSlaveHandle {
    fn new(send: oneshot::Sender<Vec<SocketAddr>>) -> ConnectionRequestSlaveHandle {
        ConnectionRequestSlaveHandle { send }
    }

    pub fn add_address_info(self, info: Vec<SocketAddr>) {
        let _ = self.send.send(info);
    }
}

pub struct ConnectionRequestMasterHandle {
    send: oneshot::Sender<()>,
}

impl ConnectionRequestMasterHandle {
    fn new(send: oneshot::Sender<()>) -> ConnectionRequestMasterHandle {
        ConnectionRequestMasterHandle { send }
    }

    pub fn relay_connection(self) {
        let _ = self.send.send(());
    }
}
