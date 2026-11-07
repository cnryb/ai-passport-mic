//! 一块音频从"收到的字节"到"声卡/文件 + 指标"的唯一通道。
//!
//! CLI 与 GUI 都只用这个结构，不许各自再写一遍长度校验、序号跳变统计与解码。
//! 之前 `record` 与 `sink` 在 `crates/cli/src/cmd.rs` 里各抄了一份，两份的
//! 丢块口径已经开始不一致 —— 这正是 `docs/GUI.md` §3 决定 D1 要避免的分叉。

use crate::session::{start, SessionHandle, SessionOptions, SessionSummary, Telemetry};
use crate::state::AppError;
use apm_link::Notification;
use apm_protocol::adpcm;
use apm_protocol::{AUDIO_BLOCK_LEN, AUDIO_BLOCK_SAMPLES};
use std::time::{Duration, Instant};

pub struct Pipeline {
    session: SessionHandle,
    blocks: u64,
    /// 序号跳变推算出的缺失块
    lost: u32,
    /// 跳变次数（一次跳变可能丢多块）
    seq_jumps: u32,
    bad_len: u32,
    device_dropped: u16,
    /// 最近一次 STATUS 报的 `LINK_SLOW`。慢档会话的欠载/漂移是链路假象，
    /// 不能当音质结论用（见 KI-005），收尾时据此给会话打 `link_slow` 标记。
    link_slow: bool,
    last_seq: Option<u16>,
    pcm: Vec<i16>,
    started: Instant,
    /// 上一块的**真到达时刻**（由 `apm_link::AudioBlock::received_at` 提供）与到达间隔
    /// 直方图（KI-006 的判据）。
    ///
    /// 计时点必须在链路层就打，不能在这里用 `Instant::now()`：本函数跑在消费循环里，
    /// 前面排着 STATUS 日志之类的工作，量到的间隔会包含我们自己的停顿。
    /// 2026-11-07 之前就是那样量的，于是"链路到达抖动"这个根因结论没有排除
    /// "PC 端自己每 12 s 卡 100 ms"这个竞争假设。
    arrival_gaps: [u64; 8],
    max_arrival_gap_ms: f64,
    late_arrivals: u64,
    /// PC 端自身处理耗时的直方图：`received_at` → 真正解码完并推入声卡的间隔。
    ///
    /// 它和 `arrival_gaps` 是两件事：前者是"我们花多久处理"，后者是"两块之间隔多久"。
    /// 同一个桶边界，直接相减就能回答"这次间隔尖峰是链路拖的还是我们自己卡的"。
    process_gaps: [u64; 8],
    max_process_gap_ms: f64,
    slow_process: u64,
    /// **第三把尺子**：每次"问链路要下一条"实际等了多久。
    ///
    /// 前两把尺子仍有一个盲区：如果消费循环在**两次读取之间**卡住（每秒一次的
    /// `println!` 写文件就是候选），数据早就躺在队列里，`received_at` 却被推迟到
    /// 我们回来才取 —— 于是"到达间隔"变长、"处理耗时"依然≈0，判词会误指链路。
    /// 记下真实的等待时长就能分开这两种情形。
    read_waits: [u64; 8],
    max_read_wait_ms: f64,
    /// 读取等待超过一个块时长（20 ms）的次数：> 0 说明我们问得不够勤。
    slow_reads: u64,
    /// 本场最长一次"间隔尖峰"期间的读取等待（ms）。判词用它区分"晚到"与"问晚了"。
    max_gap_read_wait_ms: f64,
    /// 上一块的到达时刻，仅用于计算到达间隔。
    prev_arrival: Option<Instant>,
    /// 细粒度到达间隔直方图（`FINE_BUCKETS_MS`）：判断主机是否攒批交付。
    fine_gaps: [u64; 8],
    /// 落到细桶之外的样本数（> 40 ms 的间隔）。
    fine_overflow: u64,
    /// 相邻间隔 < 5 ms 的次数 —— 攒批的直接证据（同一批里的块几乎同时到）。
    bursts_within_5ms: u64,
    /// **链路 RTT 采样**：`(相对会话起点的 ms, RTT ms)`。
    ///
    /// 为什么要和音频放在同一个结构里：判断"停顿归谁"的唯一办法是看
    /// **RTT 尖峰与到达尖峰是否发生在同一时刻**。
    /// - 两者同时尖峰 → 整条链路（含主机蓝牙栈）都堵了；
    /// - 只有到达尖峰、RTT 平稳 → 音频通知被延后，但控制面通畅 →
    ///   指向通知队列/流控（设备侧发送或主机分发），而不是射频。
    ///
    /// 定长数组，音频路径上不分配。
    rtt_samples: [(f64, f64); RTT_LOG],
    rtt_logged: usize,
    rtt_total: u64,
}

/// 到达间隔直方图的桶边界（毫秒）。上界 80 ms 是缓冲目标上限，超过它必然造成欠载。
pub const ARRIVAL_BUCKETS_MS: [f64; 8] =
    [30.0, 50.0, 60.0, 70.0, 80.0, 100.0, 200.0, f64::INFINITY];

/// 细粒度桶：专门看 0–40 ms 区间的形状，用来判断"主机是不是攒批交付"。
///
/// 为什么需要它：上面那组桶的第一档是 `0–30ms`，把"每 20 ms 来一块"（正常节奏）
/// 和"3 块挤在 1 ms 内到达"（攒批后的突发）都归进同一格，看不出区别。
///
/// 判读方式：
/// - **正常节奏**：绝大多数间隔落在 15–25 ms 一档（块天然 20 ms 一块）；
/// - **攒批交付**：出现双峰 —— 大量 <5 ms（同一批里的相邻块）+ 若干 30–40 ms
///   （两批之间的空档）。这是 CoreBluetooth 把通知攒起来再一次性交出的形状。
pub const FINE_BUCKETS_MS: [f64; 8] =
    [2.0, 5.0, 10.0, 15.0, 20.0, 25.0, 30.0, 40.0];

/// 单块处理耗时超过这个值（毫秒）就算"我们自己慢了"。
///
/// 20 ms 是块的时长：处理一块如果比来一块还慢，消费端就已经在追赶，而不是在抖动。
const SLOW_PROCESS_MS: f64 = 20.0;

/// 读取等待超过这个值（毫秒）才算"异常地久"。
///
/// **不能用 `SLOW_PROCESS_MS` 当阈值**：块每 20 ms 来一块，处理完立刻问下一条，
/// 稳态下读取就是要等约 20 ms —— 那是正常节奏，不是问题。2026-11-07 实测就踩过：
/// 用 20 ms 当阈值时报出"58% 的读取都超时"，纯属自造噪声。
/// 取 80 ms（缓冲上限）作为"这次等待已经足以吃掉存量"的口径。
const SLOW_READ_MS: f64 = 80.0;

/// 记录多少个 RTT 采样。每秒 2 次 × 600 s = 1200，取 2048 够整场。
pub const RTT_LOG: usize = 2048;

impl Pipeline {
    /// 开一条会话语义明确的管线：声卡与 WAV 都可选，但至少要有一样，
    /// 否则等于白收数据 —— 这里直接拒绝，而不是默默跑一条什么都不做的循环。
    pub fn start(opts: SessionOptions) -> Result<Self, AppError> {
        if opts.sink.is_none() && opts.wav_path.is_none() {
            return Err(AppError::busy(
                "既没有选声卡也没有勾选保存 WAV：这次会话不会产生任何输出",
            ));
        }
        let session = start(opts)?;
        Ok(Self {
            session,
            blocks: 0,
            lost: 0,
            seq_jumps: 0,
            bad_len: 0,
            device_dropped: 0,
            link_slow: false,
            last_seq: None,
            pcm: vec![0i16; AUDIO_BLOCK_SAMPLES],
            started: Instant::now(),
            arrival_gaps: [0; 8],
            max_arrival_gap_ms: 0.0,
            late_arrivals: 0,
            process_gaps: [0; 8],
            max_process_gap_ms: 0.0,
            slow_process: 0,
            read_waits: [0; 8],
            max_read_wait_ms: 0.0,
            slow_reads: 0,
            max_gap_read_wait_ms: 0.0,
            prev_arrival: None,
            fine_gaps: [0; 8],
            fine_overflow: 0,
            bursts_within_5ms: 0,
            rtt_samples: [(0.0, 0.0); RTT_LOG],
            rtt_logged: 0,
            rtt_total: 0,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session.info().session_id
    }

    pub fn sink_device(&self) -> Option<&str> {
        self.session.info().sink_device.as_deref()
    }

    pub fn wav_path(&self) -> Option<&str> {
        self.session.info().wav_path.as_deref()
    }

    /// 收一块音频。长度不对就计数并丢弃 —— 通常是 MTU 不足被截断，
    /// 静默接受会让音质报告失真。
    ///
    /// `received_at` 必须是链路层打的那个时刻（[`apm_link::AudioBlock::received_at`]），
    /// 不能用 `Instant::now()`：后者会把自己的排队/处理时间算成"到达间隔"。
    /// `read_waited` 同样来自链路层，是"问下一条等了多久"——第三把尺子。
    pub fn on_audio(&mut self, block: &[u8], received_at: Instant, read_waited: Duration) {
        self.note_read_wait(read_waited);
        self.note_process(received_at);
        if block.len() != AUDIO_BLOCK_LEN {
            self.bad_len += 1;
            self.prev_arrival = Some(received_at);
            return;
        }
        self.note_arrival(received_at, read_waited);
        if let Some((seq, _)) = adpcm::block_header(block) {
            if let Some(prev) = self.last_seq {
                let gap = seq.wrapping_sub(prev.wrapping_add(1));
                if gap != 0 {
                    self.lost += gap as u32;
                    self.seq_jumps += 1;
                }
            }
            self.last_seq = Some(seq);
        }
        adpcm::decode_into(block, &mut self.pcm);
        self.session.push(&self.pcm);
        self.blocks += 1;
    }

    /// 设备 STATUS：发送阶段丢的块序号根本没发出来，PC 端只能靠这个数。
    /// 顺带记下 `LINK_SLOW`：它一旦出现就一直算脏，直到本次会话结束
    /// （慢档期间产生的欠载/漂移无法事后剔除，整场都不可信）。
    pub fn on_status(&mut self, status: &apm_protocol::frame::Status) {
        self.device_dropped = status.dropped;
        if status.link_slow() {
            self.link_slow = true;
        }
    }

    /// 本场会话是否出现过 `LINK_SLOW`（脏会话，不能写进音质对比）。
    pub fn link_slow(&self) -> bool {
        self.link_slow
    }

    /// 记一次**链路到达**，更新间隔直方图（KI-006）。
    ///
    /// 用的是链路层打的时刻，所以这个直方图回答的是"设备/协议栈/主机蓝牙栈
    /// 交给我们时，间隔有多不匀"，**不包含**我们自己的处理时间。
    fn note_arrival(&mut self, received_at: Instant, read_waited: Duration) {
        if let Some(prev) = self.prev_arrival {
            let gap_ms = received_at
                .saturating_duration_since(prev)
                .as_secs_f64()
                * 1000.0;
            if gap_ms > self.max_arrival_gap_ms {
                self.max_arrival_gap_ms = gap_ms;
                // 记下**最长那次间隔**对应的读取等待：判词靠它区分
                // "数据晚到"（读取也等了很久）与"我们问晚了"（读取几乎不等）。
                self.max_gap_read_wait_ms = read_waited.as_secs_f64() * 1000.0;
            }
            // 超过缓冲上限（80 ms）的间隔必然先造成一次欠载
            if gap_ms > 80.0 {
                self.late_arrivals += 1;
            }
            // 细粒度分布：判断主机是否把通知攒成批再交（见 FINE_BUCKETS_MS）。
            if gap_ms < 5.0 {
                self.bursts_within_5ms += 1;
            }
            let mut binned = false;
            for (i, &edge) in FINE_BUCKETS_MS.iter().enumerate() {
                if gap_ms <= edge {
                    self.fine_gaps[i] += 1;
                    binned = true;
                    break;
                }
            }
            if !binned {
                self.fine_overflow += 1;
            }
            for (i, &edge) in ARRIVAL_BUCKETS_MS.iter().enumerate() {
                if gap_ms <= edge {
                    self.arrival_gaps[i] += 1;
                    break;
                }
            }
        }
        self.prev_arrival = Some(received_at);
    }

    /// 记一次"问下一条等了多久"（第三把尺子）。
    fn note_read_wait(&mut self, waited: Duration) {
        let ms = waited.as_secs_f64() * 1000.0;
        if ms > self.max_read_wait_ms {
            self.max_read_wait_ms = ms;
        }
        if ms > SLOW_READ_MS {
            self.slow_reads += 1;
        }
        for (i, &edge) in ARRIVAL_BUCKETS_MS.iter().enumerate() {
            if ms <= edge {
                self.read_waits[i] += 1;
                break;
            }
        }
    }

    /// 记一次**我们自己的处理耗时**：从链路层打点到此刻真正解码完。
    ///
    /// 这是 KI-006 复核要的第二把尺子。有了它和 [`Self::arrival_stats`]，一次间隔
    /// 尖峰属于"链路拖的"还是"我们自己卡的"就能直接分开判：
    /// - 到达间隔大、处理耗时小 → 链路/主机蓝牙栈问题；
    /// - 到达间隔大、处理耗时也大 → 是我们消费循环卡住（日志落盘、锁竞争、调度）。
    fn note_process(&mut self, received_at: Instant) {
        let ms = received_at.elapsed().as_secs_f64() * 1000.0;
        if ms > self.max_process_gap_ms {
            self.max_process_gap_ms = ms;
        }
        if ms > SLOW_PROCESS_MS {
            self.slow_process += 1;
        }
        for (i, &edge) in ARRIVAL_BUCKETS_MS.iter().enumerate() {
            if ms <= edge {
                self.process_gaps[i] += 1;
                break;
            }
        }
    }

    /// 块到达间隔统计：`(直方图, 最大间隔 ms, 超过 80 ms 的次数)`。
    /// 这是判断"欠载是不是链路抖动造成"的直接证据（KI-006）。
    pub fn arrival_stats(&self) -> (&[u64; 8], f64, u64) {
        (
            &self.arrival_gaps,
            self.max_arrival_gap_ms,
            self.late_arrivals,
        )
    }

    /// PC 端自身处理耗时统计：`(直方图, 最大耗时 ms, 超过 20 ms 的块数)`。
    ///
    /// 与 [`Self::arrival_stats`] 配合使用：如果这里的最大耗时也很高，
    /// 那么"链路到达抖动"这个结论就不成立 —— 抖动是我们自己制造的。
    pub fn process_stats(&self) -> (&[u64; 8], f64, u64) {
        (
            &self.process_gaps,
            self.max_process_gap_ms,
            self.slow_process,
        )
    }

    /// 读取等待统计：`(直方图, 最大等待 ms, 超过 20 ms 的次数)`。
    ///
    /// 这是第三把尺子。它回答"我们问链路问得够不够勤"：
    /// 读取等待长 = 数据确实还没到；读取等待短而到达间隔长 = 我们自己晚问了。
    pub fn read_wait_stats(&self) -> (&[u64; 8], f64, u64) {
        (
            &self.read_waits,
            self.max_read_wait_ms,
            self.slow_reads,
        )
    }

    /// 最长那次到达间隔期间，读取实际等了多久（ms）。
    ///
    /// 判词靠它区分"数据晚到"与"我们问晚了"。单独暴露是为了让报告能把
    /// 这两个数字直接摆在一起，而不是只给一个结论。
    pub fn gap_read_wait_ms(&self) -> f64 {
        self.max_gap_read_wait_ms
    }

    /// 最大到达间隔（ms）。测试与诊断包用，避免直接读私有字段。
    pub fn max_arrival_gap_ms(&self) -> f64 {
        self.max_arrival_gap_ms
    }

    /// 记一次链路 RTT 采样（相对会话起点 ms, RTT ms）。
    ///
    /// 由调用方在推流循环里定期调（`apmctl sink --rtt`）。只是把两个数字记下来，
    /// 不做任何判断 —— 判读交给看报告的人。
    pub fn note_rtt(&mut self, at: Instant, rtt: Duration) {
        self.rtt_total += 1;
        if self.rtt_logged < RTT_LOG {
            let at_ms = at.saturating_duration_since(self.started).as_secs_f64() * 1000.0;
            self.rtt_samples[self.rtt_logged] = (at_ms, rtt.as_secs_f64() * 1000.0);
            self.rtt_logged += 1;
        }
    }

    /// RTT 采样 `(相对起点 ms, RTT ms)`。
    pub fn rtt_samples(&self) -> &[(f64, f64)] {
        &self.rtt_samples[..self.rtt_logged]
    }

    /// 细粒度到达间隔 `(直方图, >40ms 的样本数, <5ms 的次数)`。
    pub fn fine_gaps(&self) -> (&[u64; 8], u64, u64) {
        (&self.fine_gaps, self.fine_overflow, self.bursts_within_5ms)
    }

    /// **通知交付模式**：主机是逐块交还是攒批交。
    ///
    /// 这是决定 G2 判据（方案 D 的阈值）该定多严的直接依据：
    /// 如果是攒批交付，那"偶尔一次长间隔"不是缺陷，而是交付机制的固有形状。
    pub fn delivery_pattern(&self) -> DeliveryPattern {
        analyze_delivery(&self.fine_gaps, self.fine_overflow, self.bursts_within_5ms)
    }

    /// 开流后的健康检查：跑够 `window` 还没收到 `min_blocks` 块，就判定
    /// "设备没在发/链路异常"，调用方应当早停。
    ///
    /// 为什么需要（KI-005）：设备停在半死态时链路是通的、STATUS 也在一秒一条地来，
    /// 但音频只有 1.5–2 块/秒。不早停就会跑满全程，产出一份"欠载上千、漂移 −9000 ppm"
    /// 的报告，看起来像声卡或漂移补偿坏了。宁可 3 s 就报错，也别让人读假数据。
    pub fn unstarted(&self, window: std::time::Duration, min_blocks: u64) -> bool {
        self.started.elapsed() >= window && self.blocks < min_blocks
    }

    /// 已收块数，供早停判据与日志用。
    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    /// 本场会话已运行时长。关麦兜底用它判"连续采集了多久"。
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// 关麦兜底的判据：已运行时长是否到达上限。
    ///
    /// 抽成纯函数是为了能被单测覆盖——逻辑藏在异步看门狗里就只能靠真机等 10 分钟，
    /// 那样的"验证"没人会真的做，等于没验。
    pub fn should_auto_stop(&self, limit: std::time::Duration) -> bool {
        self.started.elapsed() >= limit
    }

    /// 只关心音频时用这个；别的通知类型原样交回给调用方处理（日志等）。
    pub fn on_notification(&mut self, notification: &Notification) -> bool {
        match notification {
            Notification::Audio(block) => {
                self.on_audio(&block.data, block.received_at, block.read_waited);
                true
            }
            Notification::Status(status) => {
                self.on_status(status);
                false
            }
            _ => false,
        }
    }

    /// 声卡实际采样率 / 声道数（没声卡时是 16 kHz / 1）。
    pub fn rate(&self) -> u32 {
        self.session.info().rate
    }

    pub fn channels(&self) -> u16 {
        self.session.info().channels
    }

    /// 写入侧 RMS（dBFS），回环验证用。
    pub fn written_rms(&self) -> Option<f64> {
        self.session.written_rms()
    }

    pub fn retarget(&self, ms: u32) {
        self.session.retarget(ms);
    }

    /// 5 Hz 遥测快照。计数器由本层填，声卡侧的数字来自 `Meter`。
    pub fn telemetry(&self) -> Telemetry {
        let now_s = self.started.elapsed().as_secs_f64();
        let (_, _, mut t) = self.session.meter().snapshot(now_s);
        t.blocks = self.blocks;
        t.lost_blocks = self.lost;
        t.seq_jumps = self.seq_jumps;
        t.device_dropped = self.device_dropped;
        t.bad_len_blocks = self.bad_len;
        t.link_slow = self.link_slow;
        t.elapsed_ms = self.started.elapsed().as_millis() as u64;
        t
    }

    pub fn finish(self) -> SessionSummary {
        let session_id = self.session.info().session_id.clone();
        // 溢出丢块只在 Meter 里，收尾前先取一次，别让"界面显示丢了 0 块"骗人。
        let (_, _, live) = self
            .session
            .meter()
            .snapshot(self.started.elapsed().as_secs_f64());
        let mut summary = self.session.finish();
        summary.session_id = session_id;
        summary.blocks = self.blocks;
        summary.lost_blocks = self.lost;
        summary.seq_jumps = self.seq_jumps;
        summary.device_dropped = self.device_dropped;
        summary.bad_len_blocks = self.bad_len;
        summary.overflow_drops = live.overflow_drops;
        summary.link_slow = self.link_slow;
        summary.max_arrival_gap_ms = self.max_arrival_gap_ms;
        summary.late_arrivals = self.late_arrivals;
        summary.max_process_gap_ms = self.max_process_gap_ms;
        summary.slow_process_blocks = self.slow_process;
        summary.gap_read_wait_ms = self.max_gap_read_wait_ms;
        summary.max_read_wait_ms = self.max_read_wait_ms;
        let delivery = analyze_delivery(&self.fine_gaps, self.fine_overflow, self.bursts_within_5ms);
        summary.delivery_pattern = delivery.pattern.as_str().to_string();
        summary.delivery_clumped_ratio = delivery.clumped_ratio;
        summary.delivery_even_ratio = delivery.even_ratio;
        summary
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use apm_protocol::adpcm::AdpcmState;

    fn block(seq: u16) -> Vec<u8> {
        let mut st = AdpcmState::default();
        let mut bytes = adpcm::encode_block(&mut st, &vec![100i16; AUDIO_BLOCK_SAMPLES]);
        // 编码器不写序号（那是固件采集侧的职责），这里补上，构造跳变场景。
        bytes[0..2].copy_from_slice(&seq.to_le_bytes());
        bytes
    }

    #[test]
    fn rejects_session_without_any_output() {
        let err = match Pipeline::start(SessionOptions {
            session_id: "x".into(),
            sink: None,
            target_ms: 80,
            wav_path: None,
        }) {
            Ok(_) => panic!("既没有声卡也没有 WAV 时不该开出一条空管线"),
            Err(err) => err,
        };
        assert_eq!(err.code, crate::state::ErrorCode::Busy);
    }

    #[test]
    fn counts_blocks_and_seq_jumps() {
        let dir = std::env::temp_dir().join("apm-app-pipe");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.wav");
        let _ = std::fs::remove_file(&path);

        let mut p = Pipeline::start(SessionOptions {
            session_id: "pipe".into(),
            sink: None,
            target_ms: 80,
            wav_path: Some(path.clone()),
        })
        .unwrap();

        p.on_audio(&block(1), Instant::now(), Duration::ZERO);
        p.on_audio(&block(2), Instant::now(), Duration::ZERO);
        p.on_audio(&block(5), Instant::now(), Duration::ZERO); // 跳变：丢了 3、4
        let t = p.telemetry();
        assert_eq!(t.blocks, 3);
        assert_eq!(t.lost_blocks, 2);
        assert_eq!(t.seq_jumps, 1);

        let summary = p.finish();
        assert_eq!(summary.blocks, 3);
        assert_eq!(summary.lost_blocks, 2);
        assert_eq!(summary.frames, 3 * AUDIO_BLOCK_SAMPLES as u64);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn arrival_gaps_are_histogrammed() {
        // KI-006 的判据就靠这个直方图。它必须：① 第一次到达不记间隔；
        // ② 每次到达都落进恰好一个桶；③ 超过 80 ms 缓冲上限的单列计数。
        let dir = std::env::temp_dir().join("apm-app-arrival");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("arr.wav");
        let _ = std::fs::remove_file(&path);

        let mut p = Pipeline::start(SessionOptions {
            session_id: "arrival".into(),
            sink: None,
            target_ms: 80,
            wav_path: Some(path.clone()),
        })
        .unwrap();

        // 第一次到达没有"上一块"，不该产生间隔样本。
        p.on_audio(&block(1), Instant::now(), Duration::ZERO);
        let (hist, max_ms, late) = p.arrival_stats();
        assert_eq!(hist.iter().sum::<u64>(), 0, "第一块不该记间隔");
        assert_eq!(max_ms, 0.0);
        assert_eq!(late, 0);

        // 连续三块之间必然有间隔（哪怕很短），全部要落桶。
        p.on_audio(&block(2), Instant::now(), Duration::ZERO);
        p.on_audio(&block(3), Instant::now(), Duration::ZERO);
        let (hist, _max_ms, late) = p.arrival_stats();
        assert_eq!(hist.iter().sum::<u64>(), 2, "3 块之间只有 2 个间隔");
        // 测试里这三块是紧挨着喂的，间隔远小于 80 ms，不该算迟到。
        assert_eq!(late, 0, "紧挨着喂的块不该被算成迟到");

        // 每个桶边界都要能正确归类（直接测边界逻辑，避免依赖真实等待）。
        for (gap, want_idx) in [
            (5.0, 0usize),
            (30.0, 0),
            (30.1, 1),
            (50.0, 1),
            (60.0, 2),
            (70.0, 3),
            (80.0, 4),
            (100.0, 5),
            (150.0, 6),
            (500.0, 7),
        ] {
            let idx = ARRIVAL_BUCKETS_MS
                .iter()
                .position(|&edge| gap <= edge)
                .unwrap();
            assert_eq!(idx, want_idx, "{gap} ms 应落在第 {want_idx} 桶");
        }

        let _ = p.finish();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bad_length_is_counted_not_accepted() {
        let dir = std::env::temp_dir().join("apm-app-pipe2");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("q.wav");
        let _ = std::fs::remove_file(&path);

        let mut p = Pipeline::start(SessionOptions {
            session_id: "pipe2".into(),
            sink: None,
            target_ms: 80,
            wav_path: Some(path.clone()),
        })
        .unwrap();
        p.on_audio(&[0u8; 10], Instant::now(), Duration::ZERO);
        assert_eq!(p.telemetry().bad_len_blocks, 1);
        assert_eq!(p.telemetry().blocks, 0);
        let s = p.finish();
        assert_eq!(s.bad_len_blocks, 1);
        let _ = std::fs::remove_file(&path);
    }

    fn fresh(name: &str) -> (Pipeline, std::path::PathBuf) {
        let dir = std::env::temp_dir().join("apm-app-pipe-guard");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.wav"));
        let _ = std::fs::remove_file(&path);
        let p = Pipeline::start(SessionOptions {
            session_id: name.into(),
            sink: None,
            target_ms: 80,
            wav_path: Some(path.clone()),
        })
        .unwrap();
        (p, path)
    }

    /// 造一块带指定到达时刻的音频，喂满 `Pipeline`。读取等待默认记 0。
    fn feed(p: &mut Pipeline, seq: u16, received_at: Instant) {
        p.on_audio(&block(seq), received_at, Duration::ZERO);
    }

    /// 同上，但显式指定那次读取等了多久 —— 用来构造"我们问晚了"的场景。
    fn feed_waited(p: &mut Pipeline, seq: u16, received_at: Instant, read_waited: Duration) {
        p.on_audio(&block(seq), received_at, read_waited);
    }

    /// **到达间隔必须用调用方给的时刻，不能用 `Instant::now()`**（KI-006 复核的核心约束）。
    ///
    /// 构造：50 块，到达时刻严格间隔 100 ms（都在过去，喂入本身瞬间完成）。
    /// - 正确实现：读到 49 次"100 ms 间隔"，全部超过 80 ms → `late == 49`；
    /// - 旧实现（在消费端 `Instant::now()`）：喂入是瞬时的，量到的间隔接近 0，
    ///   `late == 0` —— 于是"链路明明在抖"会被量成"链路很干净"，反之亦然。
    ///
    /// 这条是纯逻辑测试，不依赖真实等待，所以在 CI 上也稳。
    #[test]
    fn arrival_gap_uses_the_given_timestamp_not_wall_clock() {
        let (mut p, path) = fresh("decoupled");

        // 全部落在过去，且间隔 100 ms。
        let base = Instant::now() - std::time::Duration::from_secs(5);
        for i in 0..50u16 {
            let arrived = base + std::time::Duration::from_millis(i as u64 * 100);
            feed(&mut p, i, arrived);
        }

        let (hist, max_ms, late) = p.arrival_stats();
        assert_eq!(hist.iter().sum::<u64>(), 49, "50 块应有 49 个间隔");
        assert_eq!(
            late, 49,
            "每个间隔都是 100 ms，全该算迟到；最大 {max_ms} ms"
        );
        assert!(
            (100.0..101.0).contains(&max_ms),
            "最大到达间隔应是 100 ms 而不是别的：{max_ms}"
        );

        let _ = p.finish();
        let _ = std::fs::remove_file(&path);
    }

    /// 处理耗时与到达间隔是两个独立的量。
    ///
    /// 构造：到达整齐 20 ms 间隔，但每块都在"到达 30 ms 之后"才被处理。
    /// 到达侧应当干净，处理侧应当全部超时 —— 两者不会互相污染。
    #[test]
    fn process_cost_is_measured_separately_from_arrival_gaps() {
        let (mut p, path) = fresh("process-separate");

        // 到达时刻都在过去，间隔 20 ms；喂入瞬间完成。
        // base 必须退得够远：120 块 × 20 ms = 2.38 s，退 5 s 才能保证全部时刻都在过去
        // （落在未来会让 `elapsed()` 饱和成 0，处理耗时被量成"零"）。
        let base = Instant::now() - std::time::Duration::from_secs(5);
        for i in 0..120u16 {
            let arrived = base + std::time::Duration::from_millis(i as u64 * 20);
            feed(&mut p, i, arrived);
        }
        let (_, arrival_max, late) = p.arrival_stats();
        assert_eq!(
            late, 0,
            "到达间隔只有 20 ms，不该有迟到（最大 {arrival_max}）"
        );
        assert!(arrival_max <= 21.0, "到达间隔被污染了：{arrival_max}");

        let (_, process_max, slow) = p.process_stats();
        assert!(
            slow > 0,
            "处理耗时超一个块时长，必须计入 slow（max={process_max}）"
        );
        assert!(
            process_max > 20.0,
            "处理耗时没有量出来（max={process_max}）"
        );

        let _ = p.finish();
        let _ = std::fs::remove_file(&path);
    }

    /// 处理侧真的慢下来时，`process_stats` 必须能把这件事量出来。
    ///
    /// 这条测试过去断言的是"判词会说 process-only"。判词已删（见 CLI 里那段注释），
    /// 但**测量本身**仍是这个模块的核心价值，所以保留测量断言：
    /// 到达整齐、处理却超时，必须体现在 `slow_process` 与 `max_process_gap_ms` 上。
    #[test]
    fn slow_processing_shows_up_in_process_stats() {
        let (mut p, path) = fresh("process-slow");
        // 到达整齐（20 ms），但都在很久之前 —— 处理侧必然超时。
        let base = Instant::now() - std::time::Duration::from_secs(10);
        for i in 0..200u16 {
            let arrived = base + std::time::Duration::from_millis(i as u64 * 20);
            feed(&mut p, i, arrived);
        }
        let (_, process_max, slow) = p.process_stats();
        assert!(slow > 0, "处理耗时超一个块时长，必须计入 slow（max={process_max}）");
        assert!(process_max > 20.0, "处理耗时没有量出来（max={process_max}）");
        // 到达间隔本身是整齐的 —— 说明"慢"确实来自处理侧，不是喂入侧。
        let (_, arrival_max, late) = p.arrival_stats();
        assert_eq!(late, 0, "到达间隔只有 20 ms，不该有迟到");
        assert!(arrival_max <= 21.0, "到达间隔被污染了：{arrival_max}");
        let _ = p.finish();
        let _ = std::fs::remove_file(&path);
    }

    /// **同一组到达间隔，读取等待不同，`gap_read_wait_ms` 必须量出差别。**
    ///
    /// 这曾用来验证"第三把尺子能把'数据晚到'和'我们问晚了'分开"，
    /// 当时断言的是两种判词（arrival-only / process-only）。判词删了，
    /// 但这两把尺子的**可区分性**仍然必须成立——否则人看报告也没法判：
    ///
    /// - 读取确实等了 140 ms → 数据**真的**晚到（`gap_read_wait_ms` ≈ 间隔）；
    /// - 读取只等了 0.2 ms → 数据早就在队列里（`gap_read_wait_ms` ≈ 0）。
    #[test]
    fn read_wait_separates_late_data_from_late_polling() {
        // 从"现在"往后递推，保证 received_at 不会落在未来（落在未来会让
        // `received_at.elapsed()` 饱和成 0，处理耗时被量成零）。
        let t0 = Instant::now() - std::time::Duration::from_secs(30);
        // 前 60 块每 20 ms 一块；后 60 块每 140 ms 一块 —— 造出 >80 ms 的到达间隔。
        let arrival_at = |i: u64| {
            if i < 60 {
                t0 + Duration::from_millis(i * 20)
            } else {
                t0 + Duration::from_millis(60 * 20 + (i - 59) * 140)
            }
        };

        // 情形 A：读取确实等了那么久 → 数据晚到。
        let (mut late_data, path_a) = fresh("late-data");
        for i in 0..120u64 {
            let arrived = arrival_at(i);
            let waited = if i < 60 {
                Duration::from_millis(20)
            } else {
                Duration::from_millis(140)
            };
            feed_waited(&mut late_data, i as u16, arrived, waited);
        }
        let (_, arrival_max_a, late_a) = late_data.arrival_stats();
        let wait_a = late_data.gap_read_wait_ms();
        assert!(late_a > 0, "140 ms 的间隔必须计入迟到");
        assert!(
            wait_a > 100.0,
            "数据真晚到时，最长那次间隔的读取等待必须很大：{wait_a} ms（间隔 {arrival_max_a} ms）"
        );

        // 情形 B：同样的到达间隔，但读取几乎不等 → 是消费循环自己卡住了。
        let (mut late_poll, path_b) = fresh("late-poll");
        for i in 0..120u64 {
            let arrived = arrival_at(i);
            let waited = if i < 60 {
                Duration::from_millis(20)
            } else {
                // 关键：只等了 0.2 ms，说明块早就躺在队列里排队了。
                Duration::from_micros(200)
            };
            feed_waited(&mut late_poll, i as u16, arrived, waited);
        }
        let (_, arrival_max_b, late_b) = late_poll.arrival_stats();
        let wait_b = late_poll.gap_read_wait_ms();
        assert_eq!(late_b, late_a, "两种情形的到达间隔应当完全一样");
        assert!(
            wait_b < 1.0,
            "读取几乎没等，说明数据早就在队列里：读等 {wait_b} ms（间隔 {arrival_max_b} ms）"
        );
        // 「我们自己花掉的」= 间隔 − 读取等待，这一项才是判读依据。
        assert!(
            (arrival_max_b - wait_b) > 100.0,
            "差额必须显示出这段时间是我们自己的：{} ms",
            arrival_max_b - wait_b
        );

        let _ = late_data.finish();
        let _ = late_poll.finish();
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// 真正干净的一场：到达时间均匀、处理即时 → 两把尺子都是干净读数。
    #[test]
    fn clean_session_has_no_late_arrivals() {
        let (mut p, path) = fresh("verdict-clean");
        // 到达时刻按 20 ms 均匀铺在过去 4 s 内，并且喂入时几乎立刻就处理
        //（received_at 与 now 的差就是处理耗时，这里刻意让它很小）。
        for i in 0..200u16 {
            feed(&mut p, i, Instant::now());
        }
        let (_, arrival_max, late) = p.arrival_stats();
        let (_, process_max, slow) = p.process_stats();
        assert_eq!(late, 0, "均匀喂入不该有迟到");
        assert_eq!(slow, 0, "处理即时就不该有慢块");
        assert!(arrival_max <= 30.0, "{arrival_max}");
        assert!(process_max <= 20.0, "{process_max}");
        let _ = p.finish();
        let _ = std::fs::remove_file(&path);
    }

    /// KI-005 的 PC 侧护栏：开流后迟迟没有音频块，必须能被判出来。
    ///
    /// 判据是"跑够窗口 + 块数不足"，两个条件同时成立才算 —— 单看块数不行，
    /// 刚开流 10 ms 时块数当然是 0，那是正常启动过程。
    #[test]
    fn detects_capture_that_never_starts() {
        use std::time::Duration;

        let (p, path) = fresh("early-stop");
        // 还没跑够窗口：哪怕一块都没有，也不能判成"没在发"。
        assert!(!p.unstarted(Duration::from_secs(3), 10), "窗口未到不该早停");
        // 窗口已 0，块数 0 < 10：这就是设备半死态的形状。
        assert!(p.unstarted(Duration::ZERO, 10), "一块没有必须判出来");
        let _ = p.finish();
        let _ = std::fs::remove_file(&path);
    }

    /// 有一点数据但远低于 50 块/秒（KI-005 实测 1.5–2 块/秒）也要拦住。
    #[test]
    fn early_stop_boundary_uses_min_blocks_not_zero() {
        use std::time::Duration;

        let (mut p, path) = fresh("early-boundary");
        for seq in 0..9u16 {
            p.on_audio(&block(seq), Instant::now(), Duration::ZERO);
        }
        assert_eq!(p.blocks(), 9);
        assert!(
            p.unstarted(Duration::ZERO, 10),
            "9 块 < 10 必须早停（1.5 块/秒的形状）"
        );

        p.on_audio(&block(9), Instant::now(), Duration::ZERO);
        assert_eq!(p.blocks(), 10);
        assert!(
            !p.unstarted(Duration::ZERO, 10),
            "达到下限就不该早停，别再动用户会话"
        );
        let _ = p.finish();
        let _ = std::fs::remove_file(&path);
    }

    /// 关麦兜底：时长到了就该停，没到就不许动用户的会话。
    ///
    /// 这条判据单独抽出来测，是因为真验要么等 10 分钟、要么改系统时钟——
    /// 两者都不会有人真做，最后等于没验。
    #[test]
    fn auto_stop_fires_only_after_limit() {
        use std::time::Duration;

        let (p, path) = fresh("auto-stop");
        // 刚开流：任何正数上限都不该触发。
        assert!(
            !p.should_auto_stop(Duration::from_secs(60)),
            "刚开流就判超时会把正常会话掐掉"
        );
        // 上限为 0：已经"到期"，这是唯一能立刻触发的形状（也是夹紧逻辑要挡的输入）。
        assert!(p.should_auto_stop(Duration::ZERO), "上限 0 必然到期");
        // 默认 10 分钟的会话语义：显然还没到。
        assert!(!p.should_auto_stop(Duration::from_secs(600)));
        let _ = p.finish();
        let _ = std::fs::remove_file(&path);
    }

    /// `LINK_SLOW` 一旦出现就一直脏：慢档期间产生的欠载无法事后剔除。
    #[test]
    fn link_slow_sticks_for_whole_session() {
        let (mut p, path) = fresh("link-slow");

        let clean = apm_protocol::frame::Status {
            state: apm_protocol::frame::state::CAPTURING,
            flags: apm_protocol::frame::FLAG_BLE_CONNECTED
                | apm_protocol::frame::FLAG_AUDIO_SUBSCRIBED,
            dropped: 0,
            sent: 0,
        };
        p.on_status(&clean);
        assert!(!p.link_slow(), "干净会话不能被误判");

        let slow = apm_protocol::frame::Status {
            flags: clean.flags | apm_protocol::frame::FLAG_LINK_SLOW,
            ..clean
        };
        p.on_status(&slow);
        assert!(p.link_slow());

        // 之后又变干净也不能洗白：这一场已经脏了。
        p.on_status(&clean);
        assert!(p.link_slow(), "LINK_SLOW 一旦置位就得粘住整场");

        let summary = p.finish();
        assert!(summary.link_slow, "收尾摘要必须带上，音质报告据此拒绝");
        let _ = std::fs::remove_file(&path);
    }
}

/// 通知交付模式 —— 主机是逐块交、还是攒成批再交。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeliveryPattern {
    /// 参与统计的间隔样本数。
    pub samples: u64,
    /// 相邻间隔 < 5 ms 的比例（同一批里的块几乎同时到）。
    pub clumped_ratio: f64,
    /// 落在 15–25 ms 的比例（块天然 20 ms 一块）。
    pub even_ratio: f64,
    /// 结论。
    pub pattern: DeliveryKind,
}

/// 交付模式的分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryKind {
    /// 样本太少。
    Insufficient,
    /// 逐块交付：绝大多数间隔落在 15–25 ms。
    Even,
    /// 攒批交付：大量 <5 ms 的相邻块 + 少量长间隔。
    Clumped,
    /// 两者都不像（例如间隔普遍偏大，可能是慢档链路）。
    Unclear,
}

impl DeliveryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Insufficient => "insufficient",
            Self::Even => "even",
            Self::Clumped => "clumped",
            Self::Unclear => "unclear",
        }
    }

    pub fn explain(&self) -> &'static str {
        match self {
            Self::Insufficient => "样本太少，不下结论",
            Self::Even => {
                "**逐块交付**：间隔集中在 20 ms 附近，主机收到一块就交一块。\
                 这种模式下长间隔属于异常事件，值得当成缺陷追"
            }
            Self::Clumped => {
                "**攒批交付**：大量块几乎同时到达（间隔 <5 ms）+ 少量长空档。\
                 这是 CoreBluetooth 把通知攒起来再一次性交出的形状，\
                 不是我们的 bug。**这种情况下\"偶尔一次长间隔\"是机制固有形状，\
                 G2 的\"0 欠载\"应当放宽为阈值型（方案 D）**"
            }
            Self::Unclear => {
                "间隔分布既不像逐块也不像攒批 —— 可能是链路慢档，先确认 STATUS 里没有 LINK_SLOW"
            }
        }
    }
}

/// 判"攒批"的门槛：相邻间隔 <5 ms 的比例超过这个值。
///
/// 20 ms 一块的正常节奏下，相邻块几乎不可能 <5 ms 到达（那需要 4 块挤在同一毫秒级窗口）。
/// 取 15% 是保守下限：攒批时这个比例通常远高（实测可达 50%+），
/// 而正常节奏下它接近 0。
pub const CLUMPED_MIN_RATIO: f64 = 0.15;

/// 判"逐块"的门槛：间隔落在 15–25 ms 的比例超过这个值。
pub const EVEN_MIN_RATIO: f64 = 0.6;

/// 至少要有这么多间隔样本才下结论。
const DELIVERY_MIN_SAMPLES: u64 = 100;

/// 分析交付模式。
///
/// `fine` 是 [`FINE_BUCKETS_MS`] 的计数，`overflow` 是 >40 ms 的数量，
/// `clumped` 是 <5 ms 的数量（与 `fine[0] + fine[1]` 一致，独立传是为了可读性）。
pub fn analyze_delivery(fine: &[u64; 8], overflow: u64, clumped: u64) -> DeliveryPattern {
    let samples: u64 = fine.iter().sum::<u64>() + overflow;
    if samples < DELIVERY_MIN_SAMPLES {
        return DeliveryPattern {
            samples,
            clumped_ratio: 0.0,
            even_ratio: 0.0,
            pattern: DeliveryKind::Insufficient,
        };
    }
    let total = samples as f64;
    let clumped_ratio = clumped as f64 / total;
    // 桶边界 [2,5,10,15,20,25,30,40]：下标 3 是 10–15、4 是 15–20、5 是 20–25。
    let even_ratio = (fine[3] + fine[4] + fine[5]) as f64 / total;

    let pattern = if clumped_ratio >= CLUMPED_MIN_RATIO {
        DeliveryKind::Clumped
    } else if even_ratio >= EVEN_MIN_RATIO {
        DeliveryKind::Even
    } else {
        DeliveryKind::Unclear
    };
    DeliveryPattern {
        samples,
        clumped_ratio,
        even_ratio,
        pattern,
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    use apm_protocol::adpcm::AdpcmState;

    /// 造一块带指定序号的音频（与 `tests::block` 同构，这里是独立模块所以复制一份）。
    fn block(seq: u16) -> Vec<u8> {
        let mut st = AdpcmState::default();
        let mut bytes = adpcm::encode_block(&mut st, &vec![100i16; AUDIO_BLOCK_SAMPLES]);
        bytes[0..2].copy_from_slice(&seq.to_le_bytes());
        bytes
    }

    /// 逐块交付：间隔几乎都在 20 ms 附近。
    #[test]
    fn even_delivery_is_recognized() {
        // 桶 [2,5,10,15,20,25,30,40]：全部落在 15–20 与 20–25。
        let mut fine = [0u64; 8];
        fine[4] = 700; // 15–20 ms
        fine[5] = 250; // 20–25 ms
        let p = analyze_delivery(&fine, 50, 0);
        assert_eq!(p.pattern, DeliveryKind::Even, "even_ratio={:.2}", p.even_ratio);
        assert_eq!(p.clumped_ratio, 0.0);
    }

    /// 攒批交付：大量 <5 ms + 一些长空档 —— 这是 CoreBluetooth 攒批的形状。
    #[test]
    fn clumped_delivery_is_recognized() {
        let mut fine = [0u64; 8];
        // 攒批：同一批里连续几块几乎同时到。
        fine[1] = 500; // 2–5 ms
        // 批与批之间的空档（>40 ms，落在 overflow）。
        let overflow = 60;
        let clumped = 500;
        let p = analyze_delivery(&fine, overflow, clumped);
        assert_eq!(
            p.pattern,
            DeliveryKind::Clumped,
            "clumped_ratio={:.2} 应判攒批",
            p.clumped_ratio
        );
        assert!(p.clumped_ratio >= CLUMPED_MIN_RATIO);
        // 判词必须说清"这不是 bug"。
        assert!(p.pattern.explain().contains("不是我们的 bug"));
    }

    /// 样本不足时不下结论 —— 30 s 的短跑不该被当成"逐块交付"的结论。
    #[test]
    fn too_few_samples_gives_no_verdict() {
        let mut fine = [0u64; 8];
        fine[4] = 50;
        assert_eq!(
            analyze_delivery(&fine, 0, 0).pattern,
            DeliveryKind::Insufficient
        );
    }

    /// 攒批的判定必须优先于逐块：真攒批数据里"15–25 ms"也可能不少（批内间隔小但批间有 20 ms 的）。
    #[test]
    fn clumped_wins_over_even_when_both_look_present() {
        let mut fine = [0u64; 8];
        fine[1] = 200; // 2–5 ms（攒批特征）
        fine[4] = 300; // 15–20（也可能是批间空档）
        fine[5] = 100;
        let p = analyze_delivery(&fine, 20, 200);
        assert_eq!(
            p.pattern,
            DeliveryKind::Clumped,
            "有 200 个 <5ms 就该判攒批（clumped={:.2}, even={:.2}）",
            p.clumped_ratio,
            p.even_ratio
        );
    }

    /// 端到端：`note_arrival` 必须真的把细粒度桶填上。
    #[test]
    fn fine_histogram_is_populated_by_arrivals() {
        let dir = std::env::temp_dir().join("apm-fine-gaps");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fine.wav");
        let _ = std::fs::remove_file(&path);
        let mut p = Pipeline::start(SessionOptions {
            session_id: "fine".into(),
            sink: None,
            target_ms: 80,
            wav_path: Some(path.clone()),
        })
        .unwrap();

        // 每块间隔 20 ms（正好落在 15–20 桶）。
        let mut t = Instant::now() - Duration::from_secs(10);
        for i in 0..120u16 {
            p.on_audio(&block(i), t, Duration::ZERO);
            t += Duration::from_millis(20);
        }
        let (fine, overflow, clumped) = p.fine_gaps();
        let total: u64 = fine.iter().sum::<u64>() + overflow;
        assert_eq!(total, 119, "120 块应有 119 个间隔");
        assert_eq!(
            fine[4], 119,
            "20 ms 间隔应全部落在 15–20 桶，实际分布 {fine:?}"
        );
        assert_eq!(clumped, 0, "20 ms 间隔不该算成攒批");
        let _ = p.finish();
        let _ = std::fs::remove_file(&path);
    }
}
