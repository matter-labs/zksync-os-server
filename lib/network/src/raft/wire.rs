use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
    InstallSnapshotResponse, VoteRequest, VoteResponse,
};
use reth_network_peers::PeerId;
use serde::{Deserialize, Serialize};
use zksync_os_consensus_types::RaftTypeConfig;

pub type RequestId = u64;
const RAFT_REQUEST_MESSAGE_ID: u8 = 0;
const RAFT_RESPONSE_MESSAGE_ID: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub enum RaftRequest {
    AppendEntries(AppendEntriesRequest<RaftTypeConfig>),
    Vote(VoteRequest<PeerId>),
    InstallSnapshot(InstallSnapshotRequest<RaftTypeConfig>),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RaftResponse {
    AppendEntries(AppendEntriesResponse<PeerId>),
    Vote(VoteResponse<PeerId>),
    InstallSnapshot(InstallSnapshotResponse<PeerId>),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RaftWireMessage {
    Request { id: RequestId, req: RaftRequest },
    Response { id: RequestId, resp: Result<RaftResponse, String> },
}

impl RaftWireMessage {
    pub fn encode(&self) -> Vec<u8> {
        #[derive(Serialize)]
        struct RequestPayload<'a> {
            id: RequestId,
            req: &'a RaftRequest,
        }

        #[derive(Serialize)]
        struct ResponsePayload<'a> {
            id: RequestId,
            resp: &'a Result<RaftResponse, String>,
        }

        let mut out = Vec::new();
        match self {
            RaftWireMessage::Request { id, req } => {
                out.push(RAFT_REQUEST_MESSAGE_ID);
                let payload = RequestPayload { id: *id, req };
                out.extend(
                    serde_json::to_vec(&payload)
                        .expect("serialize raft request payload"),
                );
            }
            RaftWireMessage::Response { id, resp } => {
                out.push(RAFT_RESPONSE_MESSAGE_ID);
                let payload = ResponsePayload { id: *id, resp };
                out.extend(
                    serde_json::to_vec(&payload)
                        .expect("serialize raft response payload"),
                );
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        #[derive(Deserialize)]
        struct RequestPayload {
            id: RequestId,
            req: RaftRequest,
        }

        #[derive(Deserialize)]
        struct ResponsePayload {
            id: RequestId,
            resp: Result<RaftResponse, String>,
        }

        let (msg_id, payload) = bytes
            .split_first()
            .ok_or_else(|| {
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "empty raft message",
                ))
            })?;

        match *msg_id {
            RAFT_REQUEST_MESSAGE_ID => {
                let payload = serde_json::from_slice::<RequestPayload>(payload)?;
                Ok(RaftWireMessage::Request {
                    id: payload.id,
                    req: payload.req,
                })
            }
            RAFT_RESPONSE_MESSAGE_ID => {
                let payload = serde_json::from_slice::<ResponsePayload>(payload)?;
                Ok(RaftWireMessage::Response {
                    id: payload.id,
                    resp: payload.resp,
                })
            }
            other => Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown raft message id: {other}"),
            ))),
        }
    }
}
