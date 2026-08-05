use crate::plugin::traits::Processor;
use crate::types::Sample;
use async_trait::async_trait;

/// 原样透传的 Processor（默认）。
pub struct Passthrough;

#[async_trait]
impl Processor for Passthrough {
    async fn process(&self, sample: Sample) -> Option<Sample> {
        Some(sample)
    }
}
