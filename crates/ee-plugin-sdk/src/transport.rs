//! Transport boundary used by native plugin entry points.

use ee_plugin_api::error::PluginError;
use ee_plugin_api::wire::{HostToPlugin, PluginToHost};

/// Message transport between a host and a native plugin process.
pub trait SdkTransport: Send {
    /// Receive the next host message, or `None` after shutdown.
    fn recv(&mut self) -> Result<Option<HostToPlugin>, PluginError>;

    /// Send a plugin message to the host.
    fn send(&mut self, message: PluginToHost) -> Result<(), PluginError>;
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::VecDeque;

    use super::*;

    /// In-memory transport used only by SDK tests.
    #[derive(Default)]
    pub struct MemoryTransport {
        incoming: VecDeque<HostToPlugin>,
        outgoing: Vec<PluginToHost>,
    }

    impl MemoryTransport {
        pub fn new(incoming: Vec<HostToPlugin>) -> Self {
            Self { incoming: incoming.into(), outgoing: Vec::new() }
        }

        pub fn outgoing(&self) -> &[PluginToHost] {
            &self.outgoing
        }
    }

    impl SdkTransport for MemoryTransport {
        fn recv(&mut self) -> Result<Option<HostToPlugin>, PluginError> {
            Ok(self.incoming.pop_front())
        }

        fn send(&mut self, message: PluginToHost) -> Result<(), PluginError> {
            self.outgoing.push(message);
            Ok(())
        }
    }

    #[test]
    fn memory_transport_round_trips_messages() {
        let mut transport = MemoryTransport::new(vec![HostToPlugin::Shutdown(
            ee_plugin_api::wire::ShutdownParams { reason: "test".to_owned() },
        )]);
        assert!(transport.recv().unwrap().is_some());
        assert!(transport.recv().unwrap().is_none());

        transport
            .send(PluginToHost::Health(ee_plugin_api::wire::HealthReport {
                state: "loaded".to_owned(),
                reason: None,
            }))
            .unwrap();
        assert_eq!(transport.outgoing().len(), 1);
    }
}
