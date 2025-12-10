use crate::TStamp;

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JobState {
    pub last_run: TStamp,
}
