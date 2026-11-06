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
use std::time::Instant;

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
    /// 上一块的到达时刻与到达间隔直方图（KI-006 的下一步：块到达抖动）。
    ///
    /// `--tone` 对照已经证明"声卡侧实时喂入 0 欠载"，所以剩下的嫌疑是
    /// **块到达间隔的抖动**：平均 50 块/秒没问题，但个别间隔超过 80 ms 的
    /// 目标缓冲就会立刻欠载一次。这个直方图就是来验这一条的。
    last_arrival: Option<Instant>,
    arrival_gaps: [u64; 8],
    max_arrival_gap_ms: f64,
    late_arrivals: u64,
}

/// 到达间隔直方图的桶边界（毫秒）。上界 80 ms 是缓冲目标上限，超过它必然造成欠载。
pub const ARRIVAL_BUCKETS_MS: [f64; 8] = [30.0, 50.0, 60.0, 70.0, 80.0, 100.0, 200.0, f64::INFINITY];

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
            last_arrival: None,
            arrival_gaps: [0; 8],
            max_arrival_gap_ms: 0.0,
            late_arrivals: 0,
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
    pub fn on_audio(&mut self, block: &[u8]) {
        if block.len() != AUDIO_BLOCK_LEN {
            self.bad_len += 1;
            return;
        }
        self.note_arrival();
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

    /// 记一次块到达，更新间隔直方图（KI-006）。
    fn note_arrival(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last_arrival {
            let gap_ms = now.saturating_duration_since(prev).as_secs_f64() * 1000.0;
            if gap_ms > self.max_arrival_gap_ms {
                self.max_arrival_gap_ms = gap_ms;
            }
            // 超过缓冲上限（80 ms）的间隔必然先造成一次欠载
            if gap_ms > 80.0 {
                self.late_arrivals += 1;
            }
            for (i, &edge) in ARRIVAL_BUCKETS_MS.iter().enumerate() {
                if gap_ms <= edge {
                    self.arrival_gaps[i] += 1;
                    break;
                }
            }
        }
        self.last_arrival = Some(now);
    }

    /// 块到达间隔统计：`(直方图, 最大间隔 ms, 超过 80 ms 的次数)`。
    /// 这是判断"欠载是不是链路抖动造成"的唯一直接证据（KI-006）。
    pub fn arrival_stats(&self) -> (&[u64; 8], f64, u64) {
        (&self.arrival_gaps, self.max_arrival_gap_ms, self.late_arrivals)
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
                self.on_audio(block);
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
        let (_, _, live) = self.session.meter().snapshot(self.started.elapsed().as_secs_f64());
        let mut summary = self.session.finish();
        summary.session_id = session_id;
        summary.blocks = self.blocks;
        summary.lost_blocks = self.lost;
        summary.seq_jumps = self.seq_jumps;
        summary.device_dropped = self.device_dropped;
        summary.bad_len_blocks = self.bad_len;
        summary.overflow_drops = live.overflow_drops;
        summary.link_slow = self.link_slow;
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

        p.on_audio(&block(1));
        p.on_audio(&block(2));
        p.on_audio(&block(5)); // 跳变：丢了 3、4
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
        p.on_audio(&block(1));
        let (hist, max_ms, late) = p.arrival_stats();
        assert_eq!(hist.iter().sum::<u64>(), 0, "第一块不该记间隔");
        assert_eq!(max_ms, 0.0);
        assert_eq!(late, 0);

        // 连续三块之间必然有间隔（哪怕很短），全部要落桶。
        p.on_audio(&block(2));
        p.on_audio(&block(3));
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
        p.on_audio(&[0u8; 10]);
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
            p.on_audio(&block(seq));
        }
        assert_eq!(p.blocks(), 9);
        assert!(p.unstarted(Duration::ZERO, 10), "9 块 < 10 必须早停（1.5 块/秒的形状）");

        p.on_audio(&block(9));
        assert_eq!(p.blocks(), 10);
        assert!(!p.unstarted(Duration::ZERO, 10), "达到下限就不该早停，别再动用户会话");
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
