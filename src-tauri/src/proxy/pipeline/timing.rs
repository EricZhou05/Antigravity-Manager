//! X-Timing-* 响应头的唯一拼装点。
//!
//! 四个适配器过去各写 5~7 条 `.header("X-Timing-...")`，字段与格式容易漂移。
//! 这里保留一个数据结构 + `Builder` 扩展，适配器只负责填入已算好的毫秒值。

use axum::http::response::Builder;

#[derive(Debug, Clone, Default)]
pub struct TimingHeaders {
    pub clean_ms: f64,
    pub norm_ms: f64,
    pub thinking_ms: f64,
    /// 单源 TTFT 口径：首个上游数据块；流式发头时写 -1。
    pub ttft_ms: f64,
    /// 响应头到达耗时；与 TTFT 分开表达，不互相冒充。
    pub headers_ms: f64,
    pub ttft_overhead_ms: f64,
    pub ttft_attempts: String,
}

impl TimingHeaders {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        clean_ms: f64,
        norm_ms: f64,
        thinking_ms: f64,
        ttft_ms: f64,
        headers_ms: f64,
        ttft_overhead_ms: f64,
        ttft_attempts: &str,
    ) -> Self {
        Self {
            clean_ms,
            norm_ms,
            thinking_ms,
            ttft_ms,
            headers_ms,
            ttft_overhead_ms,
            ttft_attempts: ttft_attempts.to_string(),
        }
    }
}

pub trait TimingBuilderExt {
    /// 写入完整的 7 个 X-Timing 头（成功响应）。
    fn timing(self, headers: &TimingHeaders) -> Self;
    /// 写入 5 个核心头 + overhead（早期错误响应）。
    fn timing_overhead(self, headers: &TimingHeaders) -> Self;
    /// 写入核心 5 个 X-Timing 头（错误/降级响应，不伪造 overhead/attempts）。
    fn timing_core(self, headers: &TimingHeaders) -> Self;
}

impl TimingBuilderExt for Builder {
    fn timing(self, h: &TimingHeaders) -> Self {
        self.timing_core(h)
            .header(
                "X-Timing-Ttft-Overhead-Ms",
                format!("{:.3}", h.ttft_overhead_ms),
            )
            .header("X-Timing-Ttft-Attempts", h.ttft_attempts.as_str())
    }

    fn timing_overhead(self, h: &TimingHeaders) -> Self {
        self.timing_core(h).header(
            "X-Timing-Ttft-Overhead-Ms",
            format!("{:.3}", h.ttft_overhead_ms),
        )
    }

    fn timing_core(self, h: &TimingHeaders) -> Self {
        self.header("X-Timing-Clean-Ms", format!("{:.3}", h.clean_ms))
            .header("X-Timing-Norm-Ms", format!("{:.3}", h.norm_ms))
            .header("X-Timing-Thinking-Ms", format!("{:.3}", h.thinking_ms))
            .header("X-Timing-Ttft-Ms", format!("{:.3}", h.ttft_ms))
            .header("X-Timing-Headers-Ms", format!("{:.3}", h.headers_ms))
    }
}
