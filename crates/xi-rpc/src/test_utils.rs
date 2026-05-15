// Copyright 2017 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Types and helpers used for testing.

use std::io::{self, Cursor};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::{self, Value};

use super::{
    Callback, Error, MessageReader, NewlineReader, Peer, ReadError, RequestId, Response, RpcObject,
    WriteTransport,
};

/// Wraps an instance of `mpsc::Sender`, implementing [`WriteTransport`].
///
/// Each `write_message` call sends the raw JSON bytes as a single string to
/// the channel.  The channel itself provides message framing, so no newline
/// or Content-Length header is added.
pub struct DummyWriter(Sender<String>);

/// Wraps an instance of `mpsc::Receiver`, providing convenience methods
/// for parsing received messages.
pub struct DummyReader(MessageReader, Receiver<String>);

/// An Peer that doesn't do anything.
#[derive(Debug, Clone)]
pub struct DummyPeer;

/// Returns a `(DummyWriter, DummyReader)` pair.
pub fn test_channel() -> (DummyWriter, DummyReader) {
    let (tx, rx) = channel();
    (DummyWriter(tx), DummyReader(MessageReader::default(), rx))
}

/// Returns a [`NewlineReader`] wrapping a [`Cursor`] over the given string.
///
/// Suitable for passing directly to [`crate::RpcLoop::mainloop`].
pub fn make_reader<S: AsRef<str>>(s: S) -> NewlineReader<Cursor<Vec<u8>>> {
    NewlineReader::new(Cursor::new(s.as_ref().as_bytes().to_vec()))
}

impl DummyReader {
    /// Attempts to read a message, returning `None` if the wait exceeds
    /// `timeout`.
    ///
    /// This method makes no assumptions about the contents of the
    /// message, and does no error handling.
    pub fn next_timeout(&mut self, timeout: Duration) -> Option<Result<RpcObject, ReadError>> {
        self.1.recv_timeout(timeout).ok().map(|s| self.0.parse(&s))
    }

    /// Reads and parses a response object.
    ///
    /// # Panics
    ///
    /// Panics if a non-response message is received, or if no message
    /// is received after a reasonable time.
    pub fn expect_response(&mut self) -> Response {
        let raw = self.next_timeout(Duration::from_secs(1)).expect("response should be received");
        let val = raw.as_ref().ok().map(|v| serde_json::to_string(&v.0));
        let resp = raw.map_err(|e| e.to_string()).and_then(|r| r.into_response());

        match resp {
            Err(msg) => panic!("Bad response: {:?}. {}", val, msg),
            Ok(resp) => resp,
        }
    }

    pub fn expect_object(&mut self) -> RpcObject {
        self.next_timeout(Duration::from_secs(1)).expect("expected object").unwrap()
    }

    pub fn expect_rpc(&mut self, method: &str) -> RpcObject {
        let obj = self
            .next_timeout(Duration::from_secs(1))
            .unwrap_or_else(|| panic!("expected rpc \"{}\"", method))
            .unwrap();
        assert_eq!(obj.get_method(), Some(method));
        obj
    }

    pub fn expect_nothing(&mut self) {
        if let Some(thing) = self.next_timeout(Duration::from_millis(500)) {
            panic!("unexpected something {:?}", thing);
        }
    }
}

impl WriteTransport for DummyWriter {
    fn write_message(&mut self, data: &[u8]) -> io::Result<()> {
        let s = String::from_utf8(data.to_vec())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.0.send(s).map_err(|err| io::Error::other(format!("{:?}", err)))
    }
}

impl Peer for DummyPeer {
    fn box_clone(&self) -> Box<dyn Peer> {
        Box::new(self.clone())
    }
    fn send_rpc_notification(&self, _method: &str, _params: &Value) {}
    fn send_rpc_request_async(
        &self,
        _method: &str,
        _params: &Value,
        f: Box<dyn Callback>,
    ) -> RequestId {
        f.call(Ok("dummy peer".into()));
        RequestId::Number(0)
    }
    fn send_rpc_request(&self, _method: &str, _params: &Value) -> Result<Value, Error> {
        Ok("dummy peer".into())
    }
    fn send_rpc_request_timeout(
        &self,
        _method: &str,
        _params: &Value,
        _timeout: std::time::Duration,
    ) -> Result<Value, Error> {
        Ok("dummy peer".into())
    }
    fn cancel_rpc_request(&self, _id: RequestId) -> bool {
        false
    }
    fn request_is_pending(&self) -> bool {
        false
    }
    fn schedule_idle(&self, _token: usize) {}
    fn schedule_timer(&self, _time: Instant, _token: usize) {}
    fn cancel_timer(&self, _token: usize) -> bool {
        false
    }

    fn request_shutdown(&self) {}
}
