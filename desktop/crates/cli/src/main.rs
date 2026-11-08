//! `apmctl` —— AI Passport Mic 的验收工具。
//!
//! 只做链路验收与音质分析，不做烧录、不做按键注入（那是 M3 的事）。
//! 每个子命令都在输出里明确区分「实测到的」与「没测的」。

mod cmd;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "apmctl", version, about = "AI Passport Mic 验收工具")]
struct Cli {
    /// 把输出重定向到这个文件。
    ///
    /// macOS 上经 LaunchServices（open）启动时拿不到 stdout，只能用文件回传；
    /// 直接跑二进制又会被 TCC 以「缺少蓝牙用途声明」abort。见 scripts/apmctl-macos.sh。
    #[arg(long, global = true)]
    log: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 扫描附近的 AI Passport Mic
    Scan {
        /// 扫描时长（秒）
        #[arg(long, default_value_t = 6)]
        timeout: u64,
    },
    /// 连接并打印设备状态（STATUS 特征值 + 1 s 心跳）
    Status {
        /// 设备名子串或 PeripheralId；不填则选第一个带本项目服务 UUID 的设备
        #[arg(long)]
        device: Option<String>,
        /// 观察时长（秒）
        #[arg(long, default_value_t = 10)]
        timeout: u64,
    },
    /// 连接并读一次电量（BLE Battery Service）；旧固件会明确提示不可用
    Battery {
        #[arg(long)]
        device: Option<String>,
    },
    /// 把按键映射推到设备屏幕（SET_KEY_LABELS），用于验收屏幕显示
    ScreenLabels {
        #[arg(long)]
        device: Option<String>,
        /// 指定按键映射 JSON；不填则用用户配置 / 内置默认
        #[arg(long)]
        keymap: Option<PathBuf>,
        /// 逐条覆盖标签文本（按 UP / DOWN / OK 顺序，最多 3 条），用于测截断与缺字形
        #[arg(long = "text")]
        text: Vec<String>,
        /// 下发后保持连接多少秒（观察屏幕自动熄屏 / 长按切屏）
        #[arg(long, default_value_t = 0)]
        hold: u64,
    },
    /// 录音到 WAV（16 kHz / 16-bit / mono）
    Record {
        /// 输出文件
        file: PathBuf,
        /// 录音时长（秒）
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long)]
        device: Option<String>,
    },
    /// 测链路往返时延（PING/PONG），不是端到端声学延迟
    Ping {
        #[arg(long)]
        device: Option<String>,
        /// 发送次数
        #[arg(long, default_value_t = 20)]
        count: u32,
        /// 两次之间的间隔（毫秒）
        #[arg(long, default_value_t = 200)]
        interval_ms: u64,
        /// 单次等待 PONG 的超时（毫秒）
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
        /// 输出 JSON 报告（schema: ai-passport-mic-ping-report-1）
        #[arg(long)]
        json: bool,
    },
    /// 把实时音频写进系统虚拟声卡（让会议软件把它当麦克风）
    Sink {
        /// 目标输出设备名子串，例如 "BlackHole 2ch"
        #[arg(long, default_value = "BlackHole")]
        to: String,
        /// 运行时长（秒）
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        /// 抖动缓冲目标（毫秒，40–80）。实测 80 最稳：60 时缓冲在 28–60 ms 间摆，80 时稳在 76–104 ms
        #[arg(long, default_value_t = 80)]
        target_ms: u32,
        /// 在同一虚拟声卡上开输入流做回环验证（需要麦克风权限）
        #[arg(long)]
        verify: bool,
        /// 不连设备，直接往声卡推 440 Hz 正弦（-20 dBFS），用来单独验证声卡直通
        #[arg(long)]
        tone: bool,
        /// 同时采集链路 RTT（PING/PONG），用于判断到达尖峰归链路还是归音频通知队列。
        /// 与录音走同一条连接、同一时间轴，不额外开连接。
        #[arg(long)]
        rtt: bool,
        #[arg(long)]
        device: Option<String>,
    },
    /// 测「OK 按下 → 首块音频」延迟（需人工按设备上的 OK 键）
    PttLatency {
        /// 需要的样本数（每按一次 OK 计一条）
        #[arg(long, default_value_t = 20)]
        count: usize,
        /// 整体超时（秒）
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// 打印每条 press / 音频块的原始信息与跳过原因（排查误配用）
        #[arg(long)]
        diagnose: bool,
        #[arg(long)]
        device: Option<String>,
    },
    /// 分析 WAV 的音质客观指标
    Analyze {
        /// 输入 WAV（PCM 16-bit）
        file: PathBuf,
        /// 输出 JSON 报告（schema: ai-passport-mic-quality-report-1）
        #[arg(long)]
        json: bool,
    },
    /// 只读一个输入设备的电平，用于排查权限与虚拟声卡直通
    Probe {
        /// 设备名子串，例如 "BlackHole" 或内置麦克风
        #[arg(long)]
        device: String,
        /// 观察时长（秒）
        #[arg(long, default_value_t = 4)]
        seconds: u64,
    },
    /// 用 C 实现导出的向量自检协议层（不需要硬件）
    Selftest {
        /// 向量文件路径；默认在仓库里自动查找 config/protocol-vectors.json
        #[arg(long)]
        vectors: Option<PathBuf>,
    },
    /// 固件烧写（走 USB，不走 BLE）：列串口 / 读版本 / 写入并校验
    #[command(subcommand)]
    Firmware(FirmwareCmd),
}

#[derive(Subcommand)]
enum FirmwareCmd {
    /// 列出疑似目标的串口。只读，不复位设备。
    Ports,
    /// 读设备当前固件版本。**会让设备复位**（正在推流会断）。
    Probe {
        /// 串口路径；只填一个候选时可以省略
        #[arg(long)]
        port: Option<String>,
        /// 合并镜像；默认用 device/build/ai-passport-mic-merged.bin
        #[arg(long)]
        image: Option<PathBuf>,
    },
    /// 写入并回读校验。**会让设备复位**。不擦 NVS。
    Flash {
        #[arg(long)]
        port: Option<String>,
        #[arg(long)]
        image: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(path) = cli.log.as_deref() {
        redirect_output(path)?;
    }

    match cli.command {
        Command::Analyze { file, json } => cmd::analyze(&file, json),
        Command::Selftest { vectors } => cmd::selftest(vectors.as_deref()),
        Command::Probe { device, seconds } => cmd::probe(&device, seconds),
        Command::Firmware(fw) => match fw {
            FirmwareCmd::Ports => cmd::firmware_ports(),
            FirmwareCmd::Probe { port, image } => {
                cmd::firmware_probe(port.as_deref(), image.as_deref())
            }
            FirmwareCmd::Flash { port, image } => {
                cmd::firmware_flash(port.as_deref(), image.as_deref())
            }
        },
        other => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                match other {
                    Command::Scan { timeout } => cmd::scan(timeout).await,
                    Command::Status { device, timeout } => {
                        cmd::status(device.as_deref(), timeout).await
                    }
                    Command::Battery { device } => cmd::battery(device.as_deref()).await,
                    Command::ScreenLabels { device, keymap, text, hold } => {
                        cmd::screen_labels(device.as_deref(), keymap.as_deref(), &text, hold)
                            .await
                    }
                    Command::Record { file, seconds, device } => {
                        cmd::record(&file, seconds, device.as_deref()).await
                    }
                    Command::Ping { device, count, interval_ms, timeout_ms, json } => {
                        cmd::ping(device.as_deref(), count, interval_ms, timeout_ms, json).await
                    }
                    Command::PttLatency { count, timeout, diagnose, device } => {
                        cmd::ptt_latency(device.as_deref(), count, timeout, diagnose).await
                    }
                    Command::Sink { to, seconds, target_ms, verify, tone, rtt, device } => {
                        cmd::sink(
                            &to,
                            seconds,
                            target_ms,
                            verify,
                            tone,
                            rtt,
                            device.as_deref(),
                        )
                        .await
                    }
                    _ => unreachable!("同步命令已在上面处理"),
                }
            })
        }
    }
}

/// 把 stdout / stderr 都改写到文件，供 LaunchServices 启动时回传结果。
fn redirect_output(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| anyhow::anyhow!("打开日志文件 {path:?} 失败: {e}"))?;
    // SAFETY: 只把标准输出/错误换成有效文件描述符，之后 f 被本进程持有到退出。
    unsafe {
        libc::dup2(f.as_raw_fd(), libc::STDOUT_FILENO);
        libc::dup2(f.as_raw_fd(), libc::STDERR_FILENO);
    }
    Ok(())
}
