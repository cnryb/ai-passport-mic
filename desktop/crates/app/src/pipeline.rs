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
    last_seq: Option<u16>,
    pcm: Vec<i16>,
    started: Instant,
}

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
            last_seq: None,
            pcm: vec![0i16; AUDIO_BLOCK_SAMPLES],
            started: Instant::now(),
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
    pub fn on_status(&mut self, status: &apm_protocol::frame::Status) {
        self.device_dropped = status.dropped;
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
}
