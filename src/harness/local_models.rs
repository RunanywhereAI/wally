//! Models downloaded on this machine, for coding tools (port of
//! src/harness/local_models.cpp).

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalModel {
    pub id: String,
    pub framework: String,
    pub dir: String,
    pub path: String,
    pub bytes: i64,
}

pub fn local_models(home: &str) -> Vec<LocalModel> {
    todo!("harness port: LocalModels ({home})")
}

pub fn local_context_size(model_id: &str) -> i64 {
    todo!("harness port: LocalContextSize ({model_id})")
}
