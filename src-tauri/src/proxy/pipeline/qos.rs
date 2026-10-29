//! 协议无关的 QoS 计量。

use crate::proxy::token_manager::TokenManager;

/// 上报并评估首包 TTFT 耗时到 QoS 守卫。
///
/// 传入的 `input_tokens` 只是 handler 侧的估值；若 Pipeline 已在**转出报文**上
/// 估过 token（四条协议口径统一），优先采用后者，避免各协议分桶标准不一致。
pub fn report_qos_ttft(
    token_manager: &TokenManager,
    account_id: &str,
    session_id: Option<&str>,
    ttft_ms: f64,
    is_deep_thinking: bool,
    input_tokens: Option<u32>,
) {
    let tier = token_manager
        .get_account_tier(account_id)
        .unwrap_or_else(|| "PRO".to_string());
    let unified_tokens = crate::proxy::pipeline::budget::estimated_tokens().or(input_tokens);
    token_manager.record_qos_ttft(
        account_id,
        session_id,
        &tier,
        ttft_ms,
        is_deep_thinking,
        unified_tokens,
    );
}
