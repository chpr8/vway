//! Control protocol client (one connection, requests answered in order).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::Value;
use vway_proto::{ErrorBody, ErrorCode, Op, Response, SessionStatus, encode_request, err};

pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    pub fn connect(session_dir: &Path) -> Result<Client, ErrorBody> {
        let path = vway_compositor::control_socket(session_dir);
        let stream = UnixStream::connect(&path).map_err(|e| {
            err(
                ErrorCode::ConnectFailed,
                format!("cannot connect to {}: {e}", path.display()),
            )
        })?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| err(ErrorCode::ConnectFailed, e.to_string()))?,
        );
        Ok(Client {
            writer: stream,
            reader,
            next_id: 1,
        })
    }

    pub fn call(&mut self, op: &Op) -> Result<Value, ErrorBody> {
        let id = self.next_id;
        self.next_id += 1;
        let line = encode_request(Some(Value::from(id)), op);
        self.writer.write_all(line.as_bytes()).map_err(|e| {
            err(
                ErrorCode::ConnectFailed,
                format!("control socket write: {e}"),
            )
        })?;
        let mut resp = String::new();
        let n = self.reader.read_line(&mut resp).map_err(|e| {
            err(
                ErrorCode::ConnectFailed,
                format!("control socket read: {e}"),
            )
        })?;
        if n == 0 {
            return Err(err(
                ErrorCode::ConnectFailed,
                "the session closed the connection (it may be shutting down)",
            ));
        }
        let resp: Response = serde_json::from_str(resp.trim_end())
            .map_err(|e| err(ErrorCode::ProtocolError, format!("bad response: {e}")))?;
        if resp.id != Some(Value::from(id)) {
            return Err(err(
                ErrorCode::ProtocolError,
                format!("response id {:?} does not match request {id}", resp.id),
            ));
        }
        resp.into_result()
    }

    pub fn call_as<T: DeserializeOwned>(&mut self, op: &Op) -> Result<T, ErrorBody> {
        let v = self.call(op)?;
        serde_json::from_value(v)
            .map_err(|e| err(ErrorCode::ProtocolError, format!("unexpected result: {e}")))
    }

    pub fn status(&mut self) -> Result<SessionStatus, ErrorBody> {
        self.call_as(&Op::Status)
    }
}
