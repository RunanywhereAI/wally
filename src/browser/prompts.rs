//! The instructions sent with every question. Kept as text files so they read
//! as prose and match Jev Ultrafast's `questions.py` byte for byte.

pub const NEXT_ACTION: &str = include_str!("prompts/next_action.txt");
pub const TARGET: &str = include_str!("prompts/target.txt");
pub const TEXT_VALUE: &str = include_str!("prompts/text_value.txt");
