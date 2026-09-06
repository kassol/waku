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
    #[serde(default)]
    pub instructions: Vec<ConsultationInstruction>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct ConsultationInstruction {
    pub delivery_id: Uuid,
    pub instruction: String,
    pub context_at: u64,
    /// Frozen before the first attempt; retries never rebuild this context.
    pub prompt: String,
    #[serde(default)]
    pub pending_targets: Vec<crate::model::StewardWaitTarget>,
    pub delivery: Option<crate::model::InputDelivery>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct ConsultationExchange {
    pub question: String,
    pub context_at: u64,
    pub answer: Option<String>,
    pub error: Option<String>,
}
