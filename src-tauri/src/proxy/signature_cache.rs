use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

// Node.js proxy uses 2 hours TTL
const SIGNATURE_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const MIN_SIGNATURE_LENGTH: usize = 32;

// Different cache limits for different layers
const TOOL_CACHE_LIMIT: usize = 10_000; // Layer 1: Aligned with SQLite MAX_TOOL_SIGNATURES_ROWS
const FAMILY_CACHE_LIMIT: usize = 200; // Layer 2: Model family mappings
const SESSION_CACHE_LIMIT: usize = 1000; // Layer 3: Session-based signatures (largest)
const TOOL_PERSIST_QUEUE_CAP: usize = 8192;
const TOOL_PERSIST_BATCH_CAP: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistMode {
    #[cfg(test)]
    MemoryOnly,
    #[allow(dead_code)]
    Sync,
    Async,
}

#[derive(Clone, Debug)]
enum ToolSigCacheValue {
    Present(String),
    Missing { global_epoch: u64, key_epoch: u64 },
}

enum ToolSigPersistCmd {
    Save {
        key: String,
        signature: String,
        seq: u64,
    },
    DeleteSession {
        session_id: String,
    },
    #[cfg(test)]
    Flush {
        ack: SyncSender<()>,
    },
}

static PERSIST_SEQ: AtomicU64 = AtomicU64::new(1);
static GLOBAL_MIN_PERSIST_SEQ: AtomicU64 = AtomicU64::new(0);
static SESSION_MIN_PERSIST_SEQ: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
static PENDING_SESSION_DELETES: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
static TOOL_SIG_PERSIST_TX: OnceLock<SyncSender<ToolSigPersistCmd>> = OnceLock::new();

#[inline]
fn next_persist_seq() -> u64 {
    PERSIST_SEQ.fetch_add(1, Ordering::AcqRel)
}

fn matches_scoped_session(scoped_key: &str, session_id: &str) -> bool {
    let scoped_prefix = format!("{session_id}\u{1f}");
    let scoped_suffix_prefix = format!(":{session_id}\u{1f}");
    scoped_key.starts_with(&scoped_prefix) || scoped_key.contains(&scoped_suffix_prefix)
}

fn matches_store_key_session(store_key: &str, session_id: &str) -> bool {
    if store_key == session_id {
        return true;
    }
    let suffix = format!(":{session_id}");
    store_key.ends_with(&suffix)
}

fn mark_pending_session_delete(session_id: &str) {
    let slot = PENDING_SESSION_DELETES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut map) = slot.lock() {
        *map.entry(session_id.to_string()).or_insert(0) += 1;
    }
}

fn complete_pending_session_delete(session_id: &str) {
    if let Some(slot) = PENDING_SESSION_DELETES.get() {
        if let Ok(mut map) = slot.lock() {
            if let Some(count) = map.get_mut(session_id) {
                if *count <= 1 {
                    map.remove(session_id);
                } else {
                    *count -= 1;
                }
            }
        }
    }
}

fn is_session_delete_pending_for_store(store_key: &str) -> bool {
    let Some(slot) = PENDING_SESSION_DELETES.get() else {
        return false;
    };
    let Ok(map) = slot.lock() else {
        return false;
    };
    map.keys()
        .any(|sid| matches_store_key_session(store_key, sid))
}

fn is_session_delete_pending_for_scoped_key(scoped_key: &str) -> bool {
    let store_key = scoped_key
        .split_once('\u{1f}')
        .map(|(prefix, _)| prefix)
        .unwrap_or(scoped_key);
    is_session_delete_pending_for_store(store_key)
}

fn flush_tool_sig_batch(batch: &mut HashMap<String, (String, u64)>) {
    if batch.is_empty() {
        return;
    }
    let entries: Vec<(String, String, u64)> =
        batch.drain().map(|(k, (sig, seq))| (k, sig, seq)).collect();
    if let Err(e) = crate::modules::proxy_db::save_tool_signatures_batch_if_current(&entries) {
        tracing::debug!("[SignatureCache] Background tool signature batch save skipped: {e}");
    }
}

fn run_tool_sig_persist_worker(rx: Receiver<ToolSigPersistCmd>) {
    let mut batch: HashMap<String, (String, u64)> = HashMap::new();
    while let Ok(first_cmd) = rx.recv() {
        let mut cur = Some(first_cmd);
        while let Some(cmd) = cur.take() {
            match cmd {
                ToolSigPersistCmd::Save {
                    key,
                    signature,
                    seq,
                } => {
                    if !is_session_delete_pending_for_scoped_key(&key)
                        && SignatureCache::is_persist_entry_current(&key, seq)
                    {
                        let should_insert = batch
                            .get(&key)
                            .map(|(_, existing_seq)| seq >= *existing_seq)
                            .unwrap_or(true);
                        if should_insert {
                            batch.insert(key, (signature, seq));
                        }
                        if batch.len() >= TOOL_PERSIST_BATCH_CAP {
                            flush_tool_sig_batch(&mut batch);
                        }
                    }
                }
                ToolSigPersistCmd::DeleteSession { session_id } => {
                    batch.retain(|k, (_, seq)| {
                        !matches_scoped_session(k, &session_id)
                            || SignatureCache::is_persist_entry_current(k, *seq)
                    });
                    flush_tool_sig_batch(&mut batch);
                    let _ =
                        crate::modules::proxy_db::delete_tool_signatures_for_session(&session_id);
                    complete_pending_session_delete(&session_id);
                }
                #[cfg(test)]
                ToolSigPersistCmd::Flush { ack } => {
                    flush_tool_sig_batch(&mut batch);
                    let _ = ack.send(());
                }
            }

            match rx.try_recv() {
                Ok(next) => cur = Some(next),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        flush_tool_sig_batch(&mut batch);
    }
}

fn tool_sig_persist_sender() -> &'static SyncSender<ToolSigPersistCmd> {
    TOOL_SIG_PERSIST_TX.get_or_init(|| {
        let (tx, rx) = sync_channel(TOOL_PERSIST_QUEUE_CAP);
        let _ = std::thread::Builder::new()
            .name("ag-tool-sig-writer".to_string())
            .spawn(move || run_tool_sig_persist_worker(rx));
        tx
    })
}

/// Cache entry with timestamp for TTL
#[derive(Clone, Debug)]
struct CacheEntry<T> {
    data: T,
    timestamp: SystemTime,
}

/// Specialized entry for session-based signatures to track message count
#[derive(Clone, Debug)]
struct SessionSignatureEntry {
    signature: String,
    message_count: usize,
}

impl<T> CacheEntry<T> {
    fn new(data: T) -> Self {
        Self {
            data,
            timestamp: SystemTime::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.timestamp.elapsed().unwrap_or(Duration::ZERO) > SIGNATURE_TTL
    }
}

/// Triple-layer signature cache to handle:
/// 1. Signature recovery for tool calls (when clients strip them)
/// 2. Cross-model compatibility checks (preventing Claude signatures on Gemini models)
/// 3. Session-based signature tracking (preventing cross-session pollution)
pub struct SignatureCache {
    /// Layer 1: Scoped Tool Use ID -> Thinking Signature (with epoch-guarded negative cache)
    /// Key: "{store_key}\x1f{normalized_tool_use_id}"
    /// Value: Present(signature) or Missing { write_epoch }
    tool_signatures: Mutex<HashMap<String, CacheEntry<ToolSigCacheValue>>>,

    /// Layer 2: Signature -> Model Family
    /// Key: thought signature string
    /// Value: Model family identifier (e.g., "claude-3-5-sonnet", "gemini-2.0-flash")
    thinking_families: Mutex<HashMap<String, CacheEntry<String>>>,

    /// Layer 3: Session ID -> Map of Message Count -> Thinking Signature (NEW)
    /// Key: session fingerprint (e.g., "sid-a1b2c3d4...")
    /// Value: A map of message count to thought signature
    /// This prevents signature pollution between different conversations and preserves history
    session_signatures: Mutex<HashMap<String, CacheEntry<HashMap<usize, SessionSignatureEntry>>>>,

    /// Layer 4: Session ID -> Assistant Reasoning Text History (NEW v4.2.0)
    /// Key: session fingerprint
    /// Value: A vector of reasoning contents (index corresponds to assistant turn index)
    session_reasonings: Mutex<HashMap<String, CacheEntry<Vec<String>>>>,

    /// L2 SQLite persistence strategy: Async background batching in production, isolated in tests.
    persist_mode: PersistMode,
}

impl SignatureCache {
    #[allow(dead_code)]
    fn new() -> Self {
        Self::with_persist_mode(PersistMode::Sync)
    }

    fn with_persist_mode(persist_mode: PersistMode) -> Self {
        Self {
            tool_signatures: Mutex::new(HashMap::new()),
            thinking_families: Mutex::new(HashMap::new()),
            session_signatures: Mutex::new(HashMap::new()),
            session_reasonings: Mutex::new(HashMap::new()),
            persist_mode,
        }
    }

    /// Global singleton instance
    pub fn global() -> &'static SignatureCache {
        static INSTANCE: OnceLock<SignatureCache> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            #[cfg(test)]
            {
                SignatureCache::with_persist_mode(PersistMode::MemoryOnly)
            }
            #[cfg(not(test))]
            {
                SignatureCache::with_persist_mode(PersistMode::Async)
            }
        })
    }

    /// Store a tool call signature
    fn scoped_tool_key(store_key: &str, tool_id: &str) -> Option<String> {
        let store_key = store_key.trim();
        let tool_id = tool_id.trim();
        if store_key.is_empty() || tool_id.is_empty() {
            return None;
        }
        Some(format!("{store_key}\u{1f}{tool_id}"))
    }

    pub(crate) fn bump_global_persist_floor() -> u64 {
        let seq = next_persist_seq();
        GLOBAL_MIN_PERSIST_SEQ.fetch_max(seq, Ordering::AcqRel);
        seq
    }

    pub(crate) fn bump_session_persist_floor(session_id: &str) -> u64 {
        let clean = session_id.trim();
        if clean.is_empty() {
            return GLOBAL_MIN_PERSIST_SEQ.load(Ordering::Acquire);
        }
        let seq = next_persist_seq();
        let slot = SESSION_MIN_PERSIST_SEQ.get_or_init(|| Mutex::new(HashMap::new()));
        if let Ok(mut map) = slot.lock() {
            let entry = map.entry(clean.to_string()).or_insert(0);
            if seq > *entry {
                *entry = seq;
            }
            if map.len() > SESSION_CACHE_LIMIT * 4 {
                let min_global = GLOBAL_MIN_PERSIST_SEQ.load(Ordering::Acquire);
                map.retain(|_, v| *v > min_global);
            }
        }
        seq
    }

    pub(crate) fn is_persist_entry_current(scoped_key: &str, seq: u64) -> bool {
        if seq <= GLOBAL_MIN_PERSIST_SEQ.load(Ordering::Acquire) {
            return false;
        }
        let Some(slot) = SESSION_MIN_PERSIST_SEQ.get() else {
            return true;
        };
        let Ok(map) = slot.lock() else {
            return true;
        };
        let store_key = scoped_key
            .split_once('\u{1f}')
            .map(|(prefix, _)| prefix)
            .unwrap_or(scoped_key);
        if let Some(&min_seq) = map.get(store_key) {
            if seq <= min_seq {
                return false;
            }
        }
        let mut rest = store_key;
        while let Some((_, tail)) = rest.split_once(':') {
            if let Some(&min_seq) = map.get(tail) {
                if seq <= min_seq {
                    return false;
                }
            }
            rest = tail;
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn allocate_persist_seq_for_test() -> u64 {
        next_persist_seq()
    }

    pub fn cache_tool_signature(&self, store_key: &str, tool_use_id: &str, signature: String) {
        let Some(signature) = crate::modules::proxy_db::normalize_and_heal_signature(&signature)
        else {
            return;
        };

        let norm_id = crate::proxy::common::utils::normalize_tool_id(tool_use_id);
        let Some(id_str) = Self::scoped_tool_key(store_key, norm_id.as_ref()) else {
            return;
        };

        // 1. 先检查并更新内存 L1 缓存：若已持有相同的未过期有效签名，直接刷新 TTL 返回，避免重复写库；
        //    若需写入，则在持有 L1 锁期间分配单调递增的 persist_seq，保证与并发会话作废严格定序。
        let persist_seq = if let Ok(mut cache) = self.tool_signatures.lock() {
            if let Some(existing) = cache.get_mut(&id_str) {
                if !existing.is_expired() {
                    if let ToolSigCacheValue::Present(ref cur_sig) = existing.data {
                        if cur_sig == &signature {
                            existing.timestamp = SystemTime::now();
                            return;
                        }
                    }
                }
            }

            let seq = next_persist_seq();
            tracing::debug!("[SignatureCache] Caching tool signature for id: {}", id_str);
            cache.insert(
                id_str.clone(),
                CacheEntry::new(ToolSigCacheValue::Present(signature.clone())),
            );

            // Clean up expired entries (and evict oldest if still above limit)
            if cache.len() > TOOL_CACHE_LIMIT {
                let before = cache.len();
                cache.retain(|_, v| !v.is_expired());
                if cache.len() > TOOL_CACHE_LIMIT {
                    let target = TOOL_CACHE_LIMIT * 4 / 5;
                    let remove_count = cache.len().saturating_sub(target);
                    let mut by_age: Vec<(String, SystemTime)> = cache
                        .iter()
                        .map(|(k, v)| (k.clone(), v.timestamp))
                        .collect();
                    by_age.sort_unstable_by_key(|(_, ts)| *ts);
                    for (k, _) in by_age.into_iter().take(remove_count) {
                        cache.remove(&k);
                    }
                }
                let after = cache.len();
                if before != after {
                    tracing::debug!(
                        "[SignatureCache] Tool cache cleanup: {} -> {} entries",
                        before,
                        after
                    );
                }
            }
            seq
        } else {
            return;
        };

        // 2. 仅对新增或变更的签名持久化到 SQLite L2 数据库 (生产环境走非阻塞批量队列，杜绝阻塞请求与 SSE 流)
        match self.persist_mode {
            PersistMode::Async => {
                if is_session_delete_pending_for_store(store_key)
                    || !Self::is_persist_entry_current(&id_str, persist_seq)
                {
                    return;
                }
                if tool_sig_persist_sender()
                    .try_send(ToolSigPersistCmd::Save {
                        key: id_str,
                        signature,
                        seq: persist_seq,
                    })
                    .is_err()
                {
                    tracing::debug!(
                        "[SignatureCache] Persist queue full, keeping tool signature in L1 only"
                    );
                }
            }
            PersistMode::Sync => {
                if Self::is_persist_entry_current(&id_str, persist_seq) {
                    let _ = crate::modules::proxy_db::save_tool_signature(&id_str, &signature);
                }
            }
            #[cfg(test)]
            PersistMode::MemoryOnly => {}
        }
    }

    /// Retrieve a signature for a tool_use_id inside one session store.
    pub fn get_tool_signature(&self, store_key: &str, tool_use_id: &str) -> Option<String> {
        let norm_id = crate::proxy::common::utils::normalize_tool_id(tool_use_id);
        let mut candidate_ids = Vec::new();
        if let Some(scoped) = Self::scoped_tool_key(store_key, norm_id.as_ref()) {
            candidate_ids.push(scoped);
        }
        if norm_id.as_ref() != tool_use_id {
            if let Some(scoped) = Self::scoped_tool_key(store_key, tool_use_id) {
                candidate_ids.push(scoped);
            }
        }
        if candidate_ids.is_empty() {
            return None;
        }

        for candidate in &candidate_ids {
            let (cur_global_epoch, cur_key_epoch) =
                crate::modules::proxy_db::current_tool_sig_epochs(candidate);

            // 1. 先查内存 L1 缓存（含按 key 精度隔离的写代数负缓存，避免历史无签名轮次反复穿透 SQLite）
            if let Ok(cache) = self.tool_signatures.lock() {
                if let Some(entry) = cache.get(candidate) {
                    if !entry.is_expired() {
                        match &entry.data {
                            ToolSigCacheValue::Present(sig) => {
                                tracing::debug!(
                                    "[SignatureCache] Hit tool signature for id: {}",
                                    candidate
                                );
                                return Some(sig.clone());
                            }
                            ToolSigCacheValue::Missing {
                                global_epoch,
                                key_epoch,
                            } if *global_epoch == cur_global_epoch
                                && *key_epoch == cur_key_epoch =>
                            {
                                continue;
                            }
                            ToolSigCacheValue::Missing { .. } => {}
                        }
                    }
                }
            }

            #[cfg(test)]
            if self.persist_mode == PersistMode::MemoryOnly {
                continue;
            }

            if self.persist_mode == PersistMode::Async
                && is_session_delete_pending_for_store(store_key)
            {
                continue;
            }

            // 2. 内存未命中（如代理重启过），从 SQLite L2 数据库恢复
            match crate::modules::proxy_db::load_tool_signature(candidate) {
                Ok(Some(sig)) => {
                    if let Ok(mut cache) = self.tool_signatures.lock() {
                        cache.insert(
                            candidate.clone(),
                            CacheEntry::new(ToolSigCacheValue::Present(sig.clone())),
                        );
                    }
                    tracing::info!(
                        "[SignatureCache] Restored tool signature from SQLite for id: {}",
                        candidate
                    );
                    return Some(sig);
                }
                Ok(None) => {
                    if let Ok(mut cache) = self.tool_signatures.lock() {
                        let occupied_by_present = cache.get(candidate).is_some_and(|e| {
                            !e.is_expired() && matches!(e.data, ToolSigCacheValue::Present(_))
                        });
                        if !occupied_by_present {
                            cache.insert(
                                candidate.clone(),
                                CacheEntry::new(ToolSigCacheValue::Missing {
                                    global_epoch: cur_global_epoch,
                                    key_epoch: cur_key_epoch,
                                }),
                            );
                        }
                    }
                }
                Err(_) => {}
            }
        }

        None
    }

    /// Store model family for a signature
    pub fn cache_thinking_family(&self, signature: String, family: String) {
        if signature.len() < MIN_SIGNATURE_LENGTH {
            return;
        }

        if let Ok(mut cache) = self.thinking_families.lock() {
            tracing::debug!(
                "[SignatureCache] Caching thinking family for sig (len={}): {}",
                signature.len(),
                family
            );
            let norm = crate::proxy::thinking_store::normalize_signature_for_comparison(&signature)
                .into_owned();
            cache.insert(signature.clone(), CacheEntry::new(family.clone()));
            if norm != signature {
                cache.insert(norm, CacheEntry::new(family));
            }

            if cache.len() > FAMILY_CACHE_LIMIT {
                let before = cache.len();
                cache.retain(|_, v| !v.is_expired());
                let after = cache.len();
                if before != after {
                    tracing::debug!(
                        "[SignatureCache] Family cache cleanup: {} -> {} entries",
                        before,
                        after
                    );
                }
            }
        }
    }

    /// Get model family for a signature
    pub fn get_signature_family(&self, signature: &str) -> Option<String> {
        if let Ok(cache) = self.thinking_families.lock() {
            if let Some(entry) = cache.get(signature) {
                if !entry.is_expired() {
                    return Some(entry.data.clone());
                } else {
                    tracing::debug!("[SignatureCache] Signature family entry expired");
                }
            }
            let alt = crate::proxy::thinking_store::normalize_signature_for_comparison(signature);
            if alt.as_ref() != signature {
                if let Some(entry) = cache.get(alt.as_ref()) {
                    if !entry.is_expired() {
                        return Some(entry.data.clone());
                    }
                }
            }
        }
        None
    }

    // ===== Layer 3: Session-based Signature Storage =====

    /// Store the thinking signature for a session at a specific message count.
    /// This is the preferred method for tracking signatures across tool loops.
    ///
    /// # Arguments
    /// * `session_id` - Session fingerprint (e.g., "sid-a1b2c3d4...")
    /// * `signature` - The thought signature to store
    /// * `message_count` - The current message count of the conversation (to detect Rewind)
    pub fn cache_session_signature(
        &self,
        session_id: &str,
        signature: String,
        message_count: usize,
    ) {
        // 自愈防裂化：若签名被误传为原始 Protobuf 二进制 (首字节 0x12)，自动纠正编码为标准 Base64
        let signature = if signature.as_bytes().first() == Some(&0x12) {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(signature.as_bytes())
        } else {
            signature
        };

        if signature.len() < MIN_SIGNATURE_LENGTH {
            return;
        }

        if let Ok(mut cache) = self.session_signatures.lock() {
            let entry = cache
                .entry(session_id.to_string())
                .or_insert_with(|| CacheEntry::new(HashMap::new()));

            // Update timestamp to refresh TTL
            entry.timestamp = SystemTime::now();

            // Detect if a rewind happened (e.g. if we have cached signatures with message_count
            // greater than the current message_count, those should be cleared since that future is gone).
            entry.data.retain(|&mc, _| {
                if mc > message_count {
                    tracing::info!(
                        "[SignatureCache] Rewind detected for {} at count {}: removing future signature at count {}.",
                        session_id,
                        message_count,
                        mc
                    );
                    false
                } else {
                    true
                }
            });

            let should_store = match entry.data.get(&message_count) {
                None => true,
                Some(existing) => {
                    // Same message count: only update if the new signature is longer (more complete)
                    signature.len() > existing.signature.len()
                }
            };

            if should_store {
                tracing::debug!(
                    "[SignatureCache] Session {} (msg_count={}) -> storing signature (len={})",
                    session_id,
                    message_count,
                    signature.len()
                );
                entry.data.insert(
                    message_count,
                    SessionSignatureEntry {
                        signature,
                        message_count,
                    },
                );
            }

            // Cleanup when limit is reached (Session cache has largest limit)
            if cache.len() > SESSION_CACHE_LIMIT {
                let before = cache.len();
                cache.retain(|_, v| !v.is_expired());
                let after = cache.len();
                if before != after {
                    tracing::info!(
                        "[SignatureCache] Session cache cleanup: {} -> {} entries (limit: {})",
                        before,
                        after,
                        SESSION_CACHE_LIMIT
                    );
                }
            }
        }
    }

    /// Retrieve the latest thinking signature for a session.
    /// Returns None if not found or expired.
    pub fn get_session_signature(&self, session_id: &str) -> Option<String> {
        if let Ok(cache) = self.session_signatures.lock() {
            // 1. 精确匹配
            if let Some(entry) = cache.get(session_id) {
                if !entry.is_expired() {
                    // Find the signature with the maximum message_count (the latest one)
                    if let Some(sig_entry) = entry.data.values().max_by_key(|e| e.message_count) {
                        tracing::debug!(
                            "[SignatureCache] Session {} (latest, msg_count={}) -> HIT (len={})",
                            session_id,
                            sig_entry.message_count,
                            sig_entry.signature.len()
                        );
                        return Some(sig_entry.signature.clone());
                    }
                } else {
                    tracing::debug!("[SignatureCache] Session {} -> EXPIRED", session_id);
                }
            }
        }
        None
    }

    /// Check if the session cache already holds a signature for a turn after `message_count`.
    pub fn has_session_signature_after(&self, session_id: &str, message_count: usize) -> bool {
        if let Ok(cache) = self.session_signatures.lock() {
            if let Some(entry) = cache.get(session_id) {
                if !entry.is_expired() {
                    return entry.data.keys().any(|&mc| mc > message_count);
                }
            }
        }
        false
    }

    /// Retrieve the thinking signature for a session at a specific message count.
    /// Returns None if not found or expired.
    pub fn get_session_signature_at(
        &self,
        session_id: &str,
        message_count: usize,
    ) -> Option<String> {
        if let Ok(cache) = self.session_signatures.lock() {
            if let Some(entry) = cache.get(session_id) {
                if !entry.is_expired() {
                    if let Some(sig_entry) = entry.data.get(&message_count) {
                        tracing::debug!(
                            "[SignatureCache] Session {} (msg_count={}) -> HIT (len={})",
                            session_id,
                            message_count,
                            sig_entry.signature.len()
                        );
                        return Some(sig_entry.signature.clone());
                    }
                }
            }
        }
        None
    }

    /// Store reasoning text for a specific assistant turn in a session
    pub fn cache_session_reasoning(&self, session_id: &str, reasoning: String, turn_index: usize) {
        if reasoning.trim().is_empty() {
            return;
        }

        if let Ok(mut cache) = self.session_reasonings.lock() {
            let entry = cache
                .entry(session_id.to_string())
                .or_insert_with(|| CacheEntry::new(Vec::new()));

            // Update timestamp to refresh TTL
            entry.timestamp = std::time::SystemTime::now();

            if turn_index >= entry.data.len() {
                entry.data.resize(turn_index + 1, String::new());
            }

            // Only update if the new reasoning is longer to prevent overwriting with partial content
            let old_len = entry.data[turn_index].len();
            if reasoning.len() > old_len {
                tracing::debug!(
                    "[SignatureCache] Session {} (turn={}) -> caching reasoning text (len: {} -> {})",
                    session_id,
                    turn_index,
                    old_len,
                    reasoning.len()
                );
                entry.data[turn_index] = reasoning;
            }

            // Session cache cleanup if limit exceeded
            if cache.len() > SESSION_CACHE_LIMIT {
                let before = cache.len();
                cache.retain(|_, v| !v.is_expired());
                let after = cache.len();
                if before != after {
                    tracing::debug!(
                        "[SignatureCache] Session reasoning cache cleanup: {} -> {} entries",
                        before,
                        after
                    );
                }
            }
        }
    }

    /// Retrieve reasoning text for a specific assistant turn in a session
    pub fn get_session_reasoning(&self, session_id: &str, turn_index: usize) -> Option<String> {
        if let Ok(cache) = self.session_reasonings.lock() {
            if let Some(entry) = cache.get(session_id) {
                if !entry.is_expired() && turn_index < entry.data.len() {
                    let text = &entry.data[turn_index];
                    if !text.trim().is_empty() {
                        tracing::debug!(
                            "[SignatureCache] Session {} (turn={}) -> Hit reasoning text cache (len: {})",
                            session_id,
                            turn_index,
                            text.len()
                        );
                        return Some(text.clone());
                    }
                }
            }
        }
        None
    }

    /// 删除指定会话的缓存签名（同步清理会话级签名、工具调用签名及底层 SQLite 持久化，保留会话推理文本）
    #[allow(dead_code)] // 预留给重试剥离、管理接口或调试使用
    pub fn delete_session_signature(&self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        let suffix = format!(":{}", session_id);
        if let Ok(mut cache) = self.session_signatures.lock() {
            let before = cache.len();
            cache.retain(|k, _| k != session_id && !k.ends_with(&suffix));
            if cache.len() != before {
                tracing::debug!(
                    "[SignatureCache] Deleted session signature for: {}",
                    session_id
                );
            }
        }
        if let Ok(mut cache) = self.tool_signatures.lock() {
            let _ = Self::bump_session_persist_floor(session_id);
            if self.persist_mode == PersistMode::Async {
                mark_pending_session_delete(session_id);
            }
            cache.retain(|k, _| !matches_scoped_session(k, session_id));
        } else {
            let _ = Self::bump_session_persist_floor(session_id);
            if self.persist_mode == PersistMode::Async {
                mark_pending_session_delete(session_id);
            }
        }
        match self.persist_mode {
            PersistMode::Async => {
                if tool_sig_persist_sender()
                    .try_send(ToolSigPersistCmd::DeleteSession {
                        session_id: session_id.to_string(),
                    })
                    .is_err()
                {
                    let _ =
                        crate::modules::proxy_db::delete_tool_signatures_for_session(session_id);
                    complete_pending_session_delete(session_id);
                }
            }
            PersistMode::Sync => {
                let _ = crate::modules::proxy_db::delete_tool_signatures_for_session(session_id);
            }
            #[cfg(test)]
            PersistMode::MemoryOnly => {}
        }
    }

    /// 彻底作废特定会话的全部签名缓存（包括 session_signatures, session_reasonings, tool_signatures 及 SQLite 持久化）
    pub fn invalidate_session(&self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        self.delete_session_signature(session_id);
        let suffix = format!(":{}", session_id);
        if let Ok(mut cache) = self.session_reasonings.lock() {
            cache.retain(|k, _| k != session_id && !k.ends_with(&suffix));
        }
    }

    /// Clear all caches (for testing or manual reset)
    #[allow(dead_code)] // Used in tests
    pub fn clear(&self) {
        let _ = Self::bump_global_persist_floor();
        if let Ok(mut cache) = self.tool_signatures.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.thinking_families.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.session_signatures.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.session_reasonings.lock() {
            cache.clear();
        }
        match self.persist_mode {
            PersistMode::Async | PersistMode::Sync => {
                let _ = crate::modules::proxy_db::clear_tool_signatures();
            }
            #[cfg(test)]
            PersistMode::MemoryOnly => {}
        }
    }

    #[cfg(test)]
    fn flush_pending_persist(&self) {
        if self.persist_mode != PersistMode::Async {
            return;
        }
        let (ack_tx, ack_rx) = sync_channel(1);
        if tool_sig_persist_sender()
            .send(ToolSigPersistCmd::Flush { ack: ack_tx })
            .is_ok()
        {
            let _ = ack_rx.recv_timeout(Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_signature_cache() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        crate::modules::proxy_db::init_db().unwrap();
        let cache = SignatureCache::new();
        let sig = "x".repeat(60); // Valid length

        cache.cache_tool_signature("scope", "tool_1", sig.clone());
        assert_eq!(cache.get_tool_signature("scope", "tool_1"), Some(sig));
        assert_eq!(cache.get_tool_signature("scope", "tool_2"), None);
        assert_eq!(cache.get_tool_signature("other", "tool_1"), None);
    }

    #[test]
    fn test_min_length() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let cache = SignatureCache::with_persist_mode(PersistMode::MemoryOnly);
        cache.cache_tool_signature("scope", "tool_short", "short".to_string());
        assert_eq!(cache.get_tool_signature("scope", "tool_short"), None);
    }

    #[test]
    fn test_thinking_family() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let cache = SignatureCache::with_persist_mode(PersistMode::MemoryOnly);
        let sig = "y".repeat(60);

        cache.cache_thinking_family(sig.clone(), "claude".to_string());
        assert_eq!(cache.get_signature_family(&sig), Some("claude".to_string()));
    }

    #[test]
    fn test_session_signature() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        let cache = SignatureCache::with_persist_mode(PersistMode::MemoryOnly);
        let sig1 = "a".repeat(60);
        let sig2 = "b".repeat(80); // Longer, should replace
        let sig3 = "c".repeat(20); // Too short (< 32), should be ignored

        // Initially empty
        assert!(cache.get_session_signature("sid-test123").is_none());

        // Store first signature
        cache.cache_session_signature("sid-test123", sig1.clone(), 5);
        assert_eq!(
            cache.get_session_signature("sid-test123"),
            Some(sig1.clone())
        );

        // Longer signature should replace (same msg count)
        cache.cache_session_signature("sid-test123", sig2.clone(), 5);
        assert_eq!(
            cache.get_session_signature("sid-test123"),
            Some(sig2.clone())
        );

        // Shorter valid signature should NOT replace (same msg count)
        cache.cache_session_signature("sid-test123", sig1.clone(), 5);
        assert_eq!(
            cache.get_session_signature("sid-test123"),
            Some(sig2.clone())
        );

        // Rewind: Shorter signature MUST replace if message count is lower
        cache.cache_session_signature("sid-test123", sig1.clone(), 3);
        assert_eq!(
            cache.get_session_signature("sid-test123"),
            Some(sig1.clone())
        );

        // Too short signature should be ignored entirely (even if rewind)
        cache.cache_session_signature("sid-test123", sig3, 1);
        assert_eq!(cache.get_session_signature("sid-test123"), Some(sig1));

        // Different session should be isolated
        assert!(cache.get_session_signature("sid-other").is_none());
    }

    #[test]
    fn test_clear_all_caches() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        crate::modules::proxy_db::init_db().unwrap();
        let cache = SignatureCache::new();
        let sig = "x".repeat(60);

        cache.cache_tool_signature("scope", "tool_1", sig.clone());
        cache.cache_thinking_family(sig.clone(), "model".to_string());
        cache.cache_session_signature("sid-1", sig.clone(), 1);

        assert!(cache.get_tool_signature("scope", "tool_1").is_some());
        assert!(cache.get_signature_family(&sig).is_some());
        assert!(cache.get_session_signature("sid-1").is_some());

        cache.clear();

        assert!(cache.get_tool_signature("scope", "tool_1").is_none());
        assert!(cache.get_signature_family(&sig).is_none());
        assert!(cache.get_session_signature("sid-1").is_none());
    }

    #[test]
    fn test_tool_signature_sqlite_recovery() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        crate::modules::proxy_db::init_db().unwrap();
        let tool_id = "call_sig_sqlite_recovery_unique";
        let sig = "s".repeat(60);

        // 1. Direct save to SQLite
        let scoped = format!("scope\u{1f}{tool_id}");
        crate::modules::proxy_db::save_tool_signature(&scoped, &sig).unwrap();

        // 2. New in-memory cache (simulating proxy restart)
        let cache = SignatureCache::new();

        // 3. get_tool_signature should fall back to SQLite and populate L1 cache
        let recovered = cache.get_tool_signature("scope", tool_id);
        assert_eq!(recovered, Some(sig.clone()));

        // 4. Second lookup should hit L1
        assert_eq!(cache.get_tool_signature("scope", tool_id), Some(sig));
    }

    #[test]
    fn test_tool_signature_normalization_match() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        crate::modules::proxy_db::init_db().unwrap();
        let cache = SignatureCache::new();
        let sig = "s".repeat(60);

        // Case A: Upstream generated call_573077, client requests with call573077
        cache.cache_tool_signature("scope", "call_573077", sig.clone());
        assert_eq!(
            cache.get_tool_signature("scope", "call573077"),
            Some(sig.clone())
        );
        assert_eq!(
            cache.get_tool_signature("scope", "call_573077"),
            Some(sig.clone())
        );

        // Case B: Client stored with call888888, lookup with call_888888
        let sig2 = "t".repeat(60);
        cache.cache_tool_signature("scope", "call888888", sig2.clone());
        assert_eq!(
            cache.get_tool_signature("scope", "call_888888"),
            Some(sig2.clone())
        );
        assert_eq!(cache.get_tool_signature("scope", "call888888"), Some(sig2));
    }

    #[test]
    fn test_invalidate_session() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        crate::modules::proxy_db::init_db().unwrap();
        let cache = SignatureCache::new();
        let sig = "s".repeat(60);

        // 1. 设置 sess-1 (带和不带租户前缀) 以及 sess-2
        cache.cache_tool_signature("sess-1", "tool_a", sig.clone());
        cache.cache_tool_signature("tenant:sess-1", "tool_b", sig.clone());
        cache.cache_tool_signature("sess-2", "tool_c", sig.clone());

        cache.cache_session_signature("sess-1", sig.clone(), 1);
        cache.cache_session_signature("tenant:sess-1", sig.clone(), 1);
        cache.cache_session_signature("sess-2", sig.clone(), 1);

        cache.cache_session_reasoning("sess-1", "reasoning 1".to_string(), 0);
        cache.cache_session_reasoning("tenant:sess-1", "reasoning tenant 1".to_string(), 0);
        cache.cache_session_reasoning("sess-2", "reasoning 2".to_string(), 0);

        // 验证已写入
        assert!(cache.get_tool_signature("sess-1", "tool_a").is_some());
        assert!(cache
            .get_tool_signature("tenant:sess-1", "tool_b")
            .is_some());
        assert!(cache.get_tool_signature("sess-2", "tool_c").is_some());

        assert!(cache.get_session_signature("sess-1").is_some());
        assert!(cache.get_session_signature("tenant:sess-1").is_some());
        assert!(cache.get_session_signature("sess-2").is_some());

        assert!(cache.get_session_reasoning("sess-1", 0).is_some());
        assert!(cache.get_session_reasoning("tenant:sess-1", 0).is_some());
        assert!(cache.get_session_reasoning("sess-2", 0).is_some());

        // 2. 清理 sess-1
        cache.invalidate_session("sess-1");

        // 3. 验证 sess-1 及其租户变体全被清除，但 sess-2 保持完好
        assert!(cache.get_tool_signature("sess-1", "tool_a").is_none());
        assert!(cache
            .get_tool_signature("tenant:sess-1", "tool_b")
            .is_none());
        assert!(cache.get_tool_signature("sess-2", "tool_c").is_some());

        assert!(cache.get_session_signature("sess-1").is_none());
        assert!(cache.get_session_signature("tenant:sess-1").is_none());
        assert!(cache.get_session_signature("sess-2").is_some());

        assert!(cache.get_session_reasoning("sess-1", 0).is_none());
        assert!(cache.get_session_reasoning("tenant:sess-1", 0).is_none());
        assert!(cache.get_session_reasoning("sess-2", 0).is_some());
    }

    #[test]
    fn test_duplicate_tool_signature_skips_redundant_sqlite_writes() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        crate::modules::proxy_db::init_db().unwrap();

        let cache = SignatureCache::new();
        let sig_v1 = "s".repeat(60);
        let sig_v2 = "u".repeat(60);

        // 1. Initial write populates both L1 and SQLite L2
        cache.cache_tool_signature("sess-dedup", "call_repeat_1", sig_v1.clone());
        let scoped_key = "sess-dedup\u{1f}call_repeat_1";
        assert_eq!(
            crate::modules::proxy_db::load_tool_signature(scoped_key).unwrap(),
            Some(sig_v1.clone())
        );

        // Pin created_at in SQLite to a sentinel value so any redundant SQLite write is caught
        let conn =
            rusqlite::Connection::open(crate::modules::proxy_db::get_proxy_db_path().unwrap())
                .unwrap();
        let sentinel_ts: i64 = chrono::Utc::now().timestamp_millis() - 60_000;
        conn.execute(
            "UPDATE tool_signatures SET created_at = ?1 WHERE tool_id = ?2",
            rusqlite::params![sentinel_ts, scoped_key],
        )
        .unwrap();

        // 2. Re-caching the identical signature 250 times (simulating multi-turn history replay)
        // must hit L1 and perform ZERO SQLite writes.
        for _ in 0..250 {
            cache.cache_tool_signature("sess-dedup", "call_repeat_1", sig_v1.clone());
        }
        let ts_after: i64 = conn
            .query_row(
                "SELECT created_at FROM tool_signatures WHERE tool_id = ?1",
                rusqlite::params![scoped_key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            ts_after, sentinel_ts,
            "Redundant tool signatures must not rewrite SQLite"
        );

        // 3. Updating to a different signature MUST persist to SQLite
        cache.cache_tool_signature("sess-dedup", "call_repeat_1", sig_v2.clone());
        assert_eq!(
            crate::modules::proxy_db::load_tool_signature(scoped_key).unwrap(),
            Some(sig_v2)
        );
    }

    #[test]
    fn test_negative_cache_invalidates_on_out_of_band_sqlite_write() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        crate::modules::proxy_db::init_db().unwrap();

        let cache = SignatureCache::new();
        let sig = "n".repeat(60);

        // 1. First miss populates per-key epoch-guarded negative cache
        assert_eq!(cache.get_tool_signature("sess-neg", "call_missing"), None);
        // 2. Writing an unrelated tool_id must NOT invalidate call_missing's negative cache
        let (global_before, key_before) =
            crate::modules::proxy_db::current_tool_sig_epochs("sess-neg\u{1f}call_missing");
        crate::modules::proxy_db::save_tool_signature("sess-neg\u{1f}call_other", &sig).unwrap();
        let (global_after_other, key_after_other) =
            crate::modules::proxy_db::current_tool_sig_epochs("sess-neg\u{1f}call_missing");
        assert_eq!(global_after_other, global_before);
        assert_eq!(key_after_other, key_before);
        assert_eq!(cache.get_tool_signature("sess-neg", "call_missing"), None);

        // 3. Direct SQLite save to call_missing bumps its per-key epoch, invalidating negative cache
        crate::modules::proxy_db::save_tool_signature("sess-neg\u{1f}call_missing", &sig).unwrap();
        assert_eq!(
            cache.get_tool_signature("sess-neg", "call_missing"),
            Some(sig)
        );
    }

    #[test]
    fn test_async_background_batch_writer_and_session_invalidation() {
        let _dir = crate::proxy::monitor::prompt_log_tests::TestDataDir::new();
        crate::modules::proxy_db::init_db().unwrap();

        let cache = SignatureCache::with_persist_mode(PersistMode::Async);
        let sig = "a".repeat(60);

        for i in 0..150 {
            cache.cache_tool_signature("sess-async-keep", &format!("call_{i}"), sig.clone());
        }
        for i in 0..50 {
            cache.cache_tool_signature("sess-async-drop", &format!("call_{i}"), sig.clone());
        }
        cache.cache_tool_signature("tenant:sess-retry-strip", "call_retry", sig.clone());
        cache.cache_session_signature("tenant:sess-retry-strip", sig.clone(), 2);
        cache.cache_session_reasoning(
            "tenant:sess-retry-strip",
            "preserved reasoning".to_string(),
            0,
        );

        // Simulate a pre-invalidation Save sequence that is delayed and arrives AFTER invalidation
        let stale_seq = SignatureCache::allocate_persist_seq_for_test();

        // Invalidate sess-async-drop and strip sess-retry-strip via delete_session_signature
        cache.invalidate_session("sess-async-drop");
        cache.delete_session_signature("sess-retry-strip");
        assert!(cache
            .get_tool_signature("sess-async-drop", "call_0")
            .is_none());
        assert!(cache
            .get_tool_signature("tenant:sess-retry-strip", "call_retry")
            .is_none());
        assert!(cache
            .get_session_signature("tenant:sess-retry-strip")
            .is_none());
        assert_eq!(
            cache.get_session_reasoning("tenant:sess-retry-strip", 0),
            Some("preserved reasoning".to_string())
        );

        // Even if an older Save command is enqueued or flushed AFTER DeleteSession completed,
        // the per-session monotonic floor must reject it and prevent resurrection.
        let _ = tool_sig_persist_sender().try_send(ToolSigPersistCmd::Save {
            key: "sess-async-drop\u{1f}call_resurrect".to_string(),
            signature: sig.clone(),
            seq: stale_seq,
        });
        let _ = tool_sig_persist_sender().try_send(ToolSigPersistCmd::Save {
            key: "tenant:sess-retry-strip\u{1f}call_retry".to_string(),
            signature: sig.clone(),
            seq: stale_seq,
        });

        cache.flush_pending_persist();

        // Verify surviving session was batch-persisted to SQLite and dropped/stripped sessions are absent
        let fresh_cache = SignatureCache::new();
        assert_eq!(
            fresh_cache.get_tool_signature("sess-async-keep", "call_0"),
            Some(sig.clone())
        );
        assert_eq!(
            fresh_cache.get_tool_signature("sess-async-keep", "call_149"),
            Some(sig.clone())
        );
        assert_eq!(
            fresh_cache.get_tool_signature("sess-async-drop", "call_0"),
            None
        );
        assert_eq!(
            fresh_cache.get_tool_signature("sess-async-drop", "call_resurrect"),
            None
        );
        assert_eq!(
            fresh_cache.get_tool_signature("tenant:sess-retry-strip", "call_retry"),
            None
        );

        // Enqueue new async writes, then call clear_all_thinking_data() directly (without cache.clear() first);
        // clear_all_thinking_data() must bump global persist floor itself so pre-clear writes cannot resurrect.
        for i in 0..40 {
            cache.cache_tool_signature("sess-async-clear", &format!("call_{i}"), sig.clone());
        }
        crate::modules::proxy_db::clear_all_thinking_data().unwrap();
        cache.clear();
        cache.flush_pending_persist();

        let post_clear_cache = SignatureCache::new();
        assert_eq!(
            post_clear_cache.get_tool_signature("sess-async-clear", "call_0"),
            None
        );
        assert_eq!(
            post_clear_cache.get_tool_signature("sess-async-keep", "call_0"),
            None
        );
    }
}
