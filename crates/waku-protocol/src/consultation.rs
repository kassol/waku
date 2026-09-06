//! Read-only discussions have their own retained history and source association.
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct Consultation {
    pub id: Uuid,
    pub source_session_id: Uuid,
    pub project_id: Uuid,
    pub context_at: u64,
    pub exchanges: Vec<ConsultationExchange>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct ConsultationExchange {
    pub question: String,
    pub context_at: u64,
    pub answer: Option<String>,
    pub error: Option<String>,
}
