//! 上游「终态失败」的统一分类与处置（协议无关的 Pipeline 横切能力）
//!
//! # 为什么放在 Pipeline
//!
//! 四个协议适配器（OpenAI Responses / OpenAI Chat Completions / Anthropic Claude /
//! Google Gemini）在上游返回 `Err` 时都必须回答同样三个问题：
//!
//! 1. 返回给客户端什么 HTTP 状态码？
//! 2. 该不该把这个账号记一次瞬态失败（30s 冷却）？
//! 3. 该不该继续换端点 / 换账号重放同一份 body？
//!
//! 这三问过去在 claude / openai(chat) / openai(responses) / gemini 各写一份，结果同一件事
//! 给出两种答案：claude 返 504，其余三个返 502；图片路径干脆没有判断，照样轮换账号。
//! 现在收敛到这里：适配器只调用一次 [`classify`]，再读 [`TerminalDisposition`]，
//! 不再自己 `contains` 内部标记，也不再自己决定状态码。
//!
//! # 标记从哪来
//!
//! 终态判定发生在唯一的上游咽喉 `upstream::client::call_v1_internal_with_headers`，
//! 它把判定结果以标记前缀写进错误文本（沿用 `Result<_, String>` 的既有形状），
//! 穿过适配器时由本模块识别。标记只用于内部链路控制，写给客户端前必须经
//! `upstream::client::strip_internal_markers` 剔除。

use axum::http::StatusCode;

/// 上传阶段被截断（上传停滞 / 上传预算耗尽 / 大 body 单次尝试策略）
pub const UPLOAD_STAGE_TERMINAL_MARKER: &str = "[upstream_upload_stage_terminal]";

/// 统一首字延迟耗尽（上游排队 / 大上下文 prefill 慢，**不是账号故障**）
pub const FIRST_BYTE_STAGE_TERMINAL_MARKER: &str = "[upstream_first_byte_terminal]";

/// 请求级全局预算耗尽（[`super::budget`] 的 270s 上限）
pub const GATEWAY_BUDGET_TERMINAL_MARKER: &str = "[upstream_gateway_budget_terminal]";

/// 同一请求内的大 body 上游调用次数达到上限（重放护栏）
pub const REPLAY_CAP_TERMINAL_MARKER: &str = "[upstream_replay_cap_terminal]";

/// 全部内部标记：写给客户端前逐一剔除
pub const ALL_TERMINAL_MARKERS: [&str; 4] = [
    UPLOAD_STAGE_TERMINAL_MARKER,
    FIRST_BYTE_STAGE_TERMINAL_MARKER,
    GATEWAY_BUDGET_TERMINAL_MARKER,
    REPLAY_CAP_TERMINAL_MARKER,
];

/// 上游终态失败的分类
///
/// 共同点：**都不是账号故障**，也都不值得再重放同一份 body。
/// 区分它们只是为了状态码与排障措辞（阶段时间线 / 日志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamTerminal {
    /// 首字节预算耗尽：上游排队或大上下文 prefill 慢
    FirstByte,
    /// 上传阶段被截断：重传只会再把同一份大 body 从头传一遍
    Upload,
    /// 全局请求预算耗尽：整条请求已经到点
    GlobalBudget,
    /// 大 body 重放次数达到上限：护栏主动拒绝
    ReplayCap,
}

/// 一次终态失败的处置结果
///
/// 适配器不再自己写 `if status == 504` / `if !first_byte_terminal`，只读这三个字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalDisposition {
    /// 返回给客户端的 HTTP 状态码
    pub status: StatusCode,
    /// 是否把账号记一次瞬态失败（进入 30s 冷却）
    pub penalize_account: bool,
    /// 是否继续换端点 / 换账号重放同一份 body
    pub stop_rotation: bool,
}

impl UpstreamTerminal {
    /// 稳定标识（日志 / 阶段时间线 / metrics 分桶用）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstByte => "first_byte_timeout",
            Self::Upload => "upload_aborted",
            Self::GlobalBudget => "gateway_budget_exhausted",
            Self::ReplayCap => "replay_cap_reached",
        }
    }

    /// 写进错误文本的内部标记
    pub fn marker(self) -> &'static str {
        match self {
            Self::FirstByte => FIRST_BYTE_STAGE_TERMINAL_MARKER,
            Self::Upload => UPLOAD_STAGE_TERMINAL_MARKER,
            Self::GlobalBudget => GATEWAY_BUDGET_TERMINAL_MARKER,
            Self::ReplayCap => REPLAY_CAP_TERMINAL_MARKER,
        }
    }

    /// 统一处置策略
    ///
    /// - 时间类终态（首字节 / 上传 / 全局预算）统一 504：客户端与监控都按
    ///   `DEADLINE_EXCEEDED` 处理，不再出现“Claude 504、其余 502”的双口径。
    /// - 重放上限是“我们主动不再重传”，与时间无关，用 503 表达“稍后重试”。
    /// - 四者都不罚账号：前三种是“等得不够久”，重放上限是“我们主动止损”，都不是账号故障。
    pub fn disposition(self) -> TerminalDisposition {
        TerminalDisposition {
            status: match self {
                Self::FirstByte | Self::Upload | Self::GlobalBudget => StatusCode::GATEWAY_TIMEOUT,
                Self::ReplayCap => StatusCode::SERVICE_UNAVAILABLE,
            },
            penalize_account: false,
            stop_rotation: true,
        }
    }
}

/// 从上游错误文本里识别终态分类；普通网络错误 / 上游 HTTP 错误返回 `None`
pub fn classify(error_text: &str) -> Option<UpstreamTerminal> {
    if error_text.contains(GATEWAY_BUDGET_TERMINAL_MARKER) {
        return Some(UpstreamTerminal::GlobalBudget);
    }
    if error_text.contains(FIRST_BYTE_STAGE_TERMINAL_MARKER) {
        return Some(UpstreamTerminal::FirstByte);
    }
    if error_text.contains(REPLAY_CAP_TERMINAL_MARKER) {
        return Some(UpstreamTerminal::ReplayCap);
    }
    if error_text.contains(UPLOAD_STAGE_TERMINAL_MARKER) {
        return Some(UpstreamTerminal::Upload);
    }
    None
}

/// 剔除仅用于内部链路控制与诊断的标记，避免泄漏到客户端错误报文
pub fn strip_markers(error_text: &str) -> String {
    let mut cleaned = error_text.to_string();
    for marker in ALL_TERMINAL_MARKERS {
        cleaned = cleaned.replace(marker, "");
    }
    cleaned
}

/// 一次客户端请求内的「结构化失败状态」聚合器
///
/// 只保留**非 429** 的最后一个失败状态：整池都被限流时才回 429，
/// 一旦混入网络 / 看门狗超时等硬失败，就以那个状态为准（避免把网关问题伪装成限流）。
#[derive(Debug, Default)]
pub struct FailureStatusTracker {
    saw_failure: bool,
    last_non_rate_limit: Option<StatusCode>,
}

impl FailureStatusTracker {
    pub fn record(&mut self, status: StatusCode) {
        self.saw_failure = true;
        if status != StatusCode::TOO_MANY_REQUESTS {
            self.last_non_rate_limit = Some(status);
        }
    }

    /// 记录一次终态失败（状态码由 [`UpstreamTerminal::disposition`] 统一给出）
    pub fn record_terminal(&mut self, terminal: UpstreamTerminal) {
        self.record(terminal.disposition().status);
    }

    pub fn final_status(&self) -> StatusCode {
        self.last_non_rate_limit.unwrap_or_else(|| {
            if self.saw_failure {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::BAD_GATEWAY
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_each_terminal_marker() {
        assert_eq!(
            classify(&format!("{}boom", FIRST_BYTE_STAGE_TERMINAL_MARKER)),
            Some(UpstreamTerminal::FirstByte)
        );
        assert_eq!(
            classify(&format!("{}boom", UPLOAD_STAGE_TERMINAL_MARKER)),
            Some(UpstreamTerminal::Upload)
        );
        assert_eq!(
            classify(&format!("{}boom", GATEWAY_BUDGET_TERMINAL_MARKER)),
            Some(UpstreamTerminal::GlobalBudget)
        );
        assert_eq!(
            classify(&format!("{}boom", REPLAY_CAP_TERMINAL_MARKER)),
            Some(UpstreamTerminal::ReplayCap)
        );
        assert_eq!(classify("ordinary network error"), None);
        assert_eq!(classify(""), None);
    }

    #[test]
    fn time_like_terminals_all_return_504_and_never_penalize() {
        for terminal in [
            UpstreamTerminal::FirstByte,
            UpstreamTerminal::Upload,
            UpstreamTerminal::GlobalBudget,
        ] {
            let disposition = terminal.disposition();
            // 四协议同口径：时间类终态一律 504（DEADLINE_EXCEEDED）
            assert_eq!(
                disposition.status,
                StatusCode::GATEWAY_TIMEOUT,
                "{terminal:?}"
            );
            assert!(!disposition.penalize_account, "{terminal:?}");
            assert!(disposition.stop_rotation, "{terminal:?}");
        }

        // 重放上限不是时间问题，用 503 表达“稍后重试”，同样不罚账号
        let replay = UpstreamTerminal::ReplayCap.disposition();
        assert_eq!(replay.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!replay.penalize_account);
        assert!(replay.stop_rotation);
    }

    #[test]
    fn strip_markers_removes_every_internal_marker() {
        let text = format!(
            "{}a {}b {}c {}d",
            UPLOAD_STAGE_TERMINAL_MARKER,
            FIRST_BYTE_STAGE_TERMINAL_MARKER,
            GATEWAY_BUDGET_TERMINAL_MARKER,
            REPLAY_CAP_TERMINAL_MARKER
        );
        let cleaned = strip_markers(&text);
        for marker in ALL_TERMINAL_MARKERS {
            assert!(!cleaned.contains(marker), "{marker} leaked: {cleaned}");
        }
    }

    #[test]
    fn failure_tracker_prefers_non_rate_limit_status() {
        let mut only_429 = FailureStatusTracker::default();
        only_429.record(StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(only_429.final_status(), StatusCode::TOO_MANY_REQUESTS);

        let mut mixed = FailureStatusTracker::default();
        mixed.record(StatusCode::TOO_MANY_REQUESTS);
        mixed.record_terminal(UpstreamTerminal::FirstByte);
        assert_eq!(mixed.final_status(), StatusCode::GATEWAY_TIMEOUT);

        // 没有任何失败记录时回 502（保持既有语义）
        assert_eq!(
            FailureStatusTracker::default().final_status(),
            StatusCode::BAD_GATEWAY
        );
    }
}
