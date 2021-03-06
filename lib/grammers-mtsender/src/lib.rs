// Copyright 2020 - developers of the `grammers` project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
use grammers_crypto::{auth_key, AuthKey};
use grammers_mtproto::errors::{RPCError, RequestError};
use grammers_mtproto::transports::{Transport, TransportFull};
use grammers_mtproto::MTProto;
pub use grammers_mtproto::DEFAULT_COMPRESSION_THRESHOLD;
use grammers_tl_types::{Deserializable, RPC};

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const DEFAULT_TIMEOUT: Option<Duration> = Some(Duration::from_secs(10));

/// The invocation might fail due to network problems, in which case the
/// outermost result represents failure.
///
/// If the request is both sent and received successfully, then the request
/// itself was understood by the server, but it could not be executed. This
/// is represented by the innermost result.
pub type RequestResult<R> = io::Result<Result<R, RPCError>>;

/// A builder to configure `MTSender` instances.
pub struct MTSenderBuilder {
    compression_threshold: Option<usize>,
    auth_key: Option<AuthKey>,
    timeout: Option<Duration>,
}

/// A Mobile Transport sender, using the [Mobile Transport Protocol]
/// underneath.
///
/// [Mobile Transport Protocol]: https://core.telegram.org/mtproto
pub struct MTSender {
    protocol: MTProto,
    stream: TcpStream,
    // TODO let the user change the type of transport used
    transport: TransportFull,
}

impl MTSenderBuilder {
    fn new() -> Self {
        Self {
            compression_threshold: DEFAULT_COMPRESSION_THRESHOLD,
            auth_key: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Configures the compression threshold for outgoing messages.
    pub fn compression_threshold(mut self, threshold: Option<usize>) -> Self {
        self.compression_threshold = threshold;
        self
    }

    /// Sets the authorization key to be used. Otherwise, no authorization
    /// key will be present, and a new one will have to be generated before
    /// being able to send encrypted messages.
    pub fn auth_key(mut self, auth_key: AuthKey) -> Self {
        self.auth_key = Some(auth_key);
        self
    }

    /// Configures the network timeout to use when performing network
    /// operations.
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Finishes the builder and returns the `MTProto` instance with all
    /// the configuration changes applied.
    pub fn connect<A: ToSocketAddrs>(self, addr: A) -> io::Result<MTSender> {
        MTSender::with_builder(self, addr)
    }
}

impl MTSender {
    /// Returns a builder to configure certain parameters.
    pub fn build() -> MTSenderBuilder {
        MTSenderBuilder::new()
    }

    /// Creates and connects a new instance with default settings.
    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        Self::build().connect(addr)
    }

    /// Constructs an instance using a finished builder.
    fn with_builder<A: ToSocketAddrs>(builder: MTSenderBuilder, addr: A) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(builder.timeout)?;

        let mut protocol = MTProto::build().compression_threshold(builder.compression_threshold);

        if let Some(auth_key) = builder.auth_key {
            protocol = protocol.auth_key(auth_key);
        }

        Ok(Self {
            protocol: protocol.finish(),
            stream,
            transport: TransportFull::new(),
        })
    }

    /// Performs the handshake necessary to generate a new authorization
    /// key that can be used to safely transmit data to and from the server.
    ///
    /// See also: https://core.telegram.org/mtproto/auth_key.
    pub fn generate_auth_key(&mut self) -> io::Result<AuthKey> {
        let (request, data) = auth_key::generation::step1()?;
        let response = self.invoke_plain_request(&request)?;

        let (request, data) = auth_key::generation::step2(data, response)?;
        let response = self.invoke_plain_request(&request)?;

        let (request, data) = auth_key::generation::step3(data, response)?;
        let response = self.invoke_plain_request(&request)?;

        let (auth_key, time_offset) = auth_key::generation::create_key(data, response)?;
        self.protocol.set_auth_key(auth_key.clone(), time_offset);

        Ok(auth_key)
    }

    /// Changes the authorization key data for a different one.
    pub fn set_auth_key(&mut self, data: [u8; 256]) {
        self.protocol.set_auth_key(AuthKey::from_bytes(data), 0);
    }

    /// Invoke a serialized request in plaintext.
    fn invoke_plain_request(&mut self, request: &[u8]) -> io::Result<Vec<u8>> {
        // Send
        let payload = self.protocol.serialize_plain_message(request);
        self.transport.send(&mut self.stream, &payload)?;

        // Receive
        let response = self.receive_message()?;
        self.protocol
            .deserialize_plain_message(&response)
            .map(|x| x.to_vec())
    }

    /// Block invoking a single Remote Procedure Call and return its result.
    ///
    /// The invocation might fail due to network problems, in which case the
    /// outermost result represents failure.
    ///
    /// If the request is both sent and received successfully, then the
    /// request itself was understood by the server, but it could not be
    /// executed. This is represented by the innermost result.
    pub fn invoke<R: RPC>(&mut self, request: &R) -> RequestResult<R::Return> {
        let mut msg_id = self.protocol.enqueue_request(request.to_bytes())?;
        loop {
            // The protocol may generate more outgoing requests, so we need
            // to constantly check for those until we receive a response.
            while let Some(payload) = self.protocol.serialize_encrypted_messages()? {
                self.transport.send(&mut self.stream, &payload)?;
            }

            // Process all messages we receive.
            let response = self.receive_message()?;
            self.protocol.process_encrypted_response(&response)?;

            // See if there are responses to our request.
            while let Some((response_id, data)) = self.protocol.poll_response() {
                if response_id == msg_id {
                    match data {
                        Ok(x) => {
                            return Ok(Ok(R::Return::from_bytes(&x)?));
                        }
                        Err(RequestError::RPCError(error)) => {
                            return Ok(Err(error));
                        }
                        Err(RequestError::BadMessage { .. }) => {
                            // Need to retransmit
                            msg_id = self.protocol.enqueue_request(request.to_bytes())?;
                        }
                    }
                }
            }
        }
    }

    /// Receives a single message from the server
    fn receive_message(&mut self) -> io::Result<Vec<u8>> {
        self.transport
            .receive(&mut self.stream)
            .map_err(|e| match e.kind() {
                io::ErrorKind::UnexpectedEof => io::Error::new(io::ErrorKind::ConnectionReset, e),
                _ => e,
            })
    }
}
