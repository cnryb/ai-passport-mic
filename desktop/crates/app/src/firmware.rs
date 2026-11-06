//! 固件烧写（走 USB，不走 BLE）。
//!
//! 契约在 [`docs/ARCHITECTURE.md`](../../../docs/ARCHITECTURE.md) 的「固件烧写与版本比对」，
//! 产品口径在 [`docs/PRODUCT.md`](../../../docs/PRODUCT.md) 的「开箱体验」。改这里之前先改文档。
//!
//! 三条不能破的约束：
//!
//! 1. **不擦 NVS。** 合并镜像里 `0x9000`–`0xFFFF` 是 `0xFF` 填充，整段写下去会把用户的
//!    按键映射、蓝牙配对和 RF 校准一起抹掉。所以按三段切着写，中间那段一个字都不碰。
//! 2. **不信任写操作的应答。** 写完用 esptool 的 FlashMd5 命令让设备自己算一遍分区内容，
//!    与本地算的 MD5 比对。供电不稳、劣质线材、无源 HUB 都会静默丢字节。
//! 3. **只动自己的设备。** VID/PID 先筛一遍，再让 espflash 用 `Chip::Esp32c3` 做芯片级确认；
//!    `0x303A:0x1001` 同时也是 ESP32-S3 的 USB-Serial-JTAG 标识，光看 VID/PID 不够。
//!
//! # 副作用
//!
//! [`probe`] 和 [`flash`] 都会让设备**复位进入下载模式**（esptool 的 `--before default_reset`
//! 就是这么工作的）。正在 BLE 推流时调用会中断会话。调用方要在界面上说清楚，不要做成
//! 后台轮询——那会让用户的设备莫名其妙掉线。

use crate::state::{AppError, ErrorCode};
use anyhow::{bail, Context, Result};
use espflash::connection::{Connection, ResetAfterOperation, ResetBeforeOperation};
use espflash::flasher::Flasher;
use espflash::image_format::Segment;
use espflash::target::{Chip, ProgressCallbacks};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 乐鑫 USB-Serial-JTAG 的 USB 标识。**ESP32-S3 也是这个 PID**，所以还要靠芯片型号确认。
pub const TARGET_VID: u16 = 0x303A;
pub const TARGET_PID: u16 = 0x1001;

/// flash 布局，与 `device/partitions.csv` 一致。改分区表必须同时改这里。
pub const BOOTLOADER_OFFSET: u32 = 0x0000;
pub const PARTITION_TABLE_OFFSET: u32 = 0x8000;
/// nvs 起始。**烧写时从这里跳过**，一直跳到 `APP_OFFSET`。
pub const NVS_OFFSET: u32 = 0x9000;
pub const PHY_INIT_OFFSET: u32 = 0xF000;
pub const APP_OFFSET: u32 = 0x10000;

/// `esp_app_desc_t` 的 magic（`0xABCD5432`，小端读出来是 `32 54 CD AB`）。
const APP_DESC_MAGIC: u32 = 0xABCD_5432;
/// 描述符在 app 镜像内的偏移：`esp_image_header_t`(24) + `esp_image_segment_header_t`(8)。
const APP_DESC_IN_APP: u32 = 0x20;
/// `version[32]` 在描述符内的偏移：magic(4) + secure_version(4) + reserv1(8)。
const VERSION_IN_DESC: u32 = 0x10;
const VERSION_LEN: usize = 32;

const PARTITION_ENTRY_LEN: usize = 32;
/// `ESP_PARTITION_MAGIC`。注意是 `0x50AA`，不是 `0xAA50`——在镜像里按小端存成
/// `AA 50` 两个字节，`u16::from_le_bytes([0xAA, 0x50])` 得到的才是它。
/// 这个字节序被真实产物测试抓到过一次，别再"顺手改回去"。
const PARTITION_MAGIC: u16 = 0x50AA;
const PARTITION_TYPE_APP: u8 = 0x00;
const PARTITION_TYPE_DATA: u8 = 0x01;
const PARTITION_SUBTYPE_FACTORY: u8 = 0x00;
const PARTITION_SUBTYPE_NVS: u8 = 0x02;

/// 烧写用的波特率。921600 更快但劣质线材/无源 HUB 上更容易失败；460800 是折中。
const FLASH_BAUD: u32 = 460_800;
/// 连接是 115200，esptool 握手要求。
const CONNECT_BAUD: u32 = 115_200;
/// 读描述的流式分块：4 KiB / 允许 64 块在飞（espflash 自己的默认值）。
const READ_BLOCK_SIZE: u32 = 0x1000;
const READ_MAX_IN_FLIGHT: u32 = 64;

/// 描述符在 flash 里的绝对地址：`0x10000 + 0x20 = 0x10020`。
pub const APP_DESC_ADDR: u32 = APP_OFFSET + APP_DESC_IN_APP;
/// 版本字段在 flash 里的绝对地址：`0x10020 + 0x10 = 0x10030`。
pub const VERSION_ADDR: u32 = APP_DESC_ADDR + VERSION_IN_DESC;

// ---------------------------------------------------------------------------
// 对外类型（都能跨 IPC，不含原始字节）
// ---------------------------------------------------------------------------

/// 一个候选串口。只包含界面要显示的信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortView {
    pub path: String,
    pub serial_number: Option<String>,
}

/// 随包镜像的元信息。**从镜像本身解析**，不从文件名或构建脚本猜。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInfo {
    pub path: PathBuf,
    pub version: String,
    pub bytes: u64,
    pub md5: String,
}

/// 设备相对随包镜像的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    /// 版本一致，不用烧。
    UpToDate,
    /// 空片（分区里读不到描述符，或整片 `0xFF`）。
    Blank,
    /// 有固件但版本不同。
    Mismatch,
    /// 读到了，但版本字符串没法比较（旧固件或损坏）。
    Unknown,
}

/// 检测结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub port: String,
    pub chip: String,
    pub current_version: Option<String>,
    pub image_version: String,
    pub state: DeviceState,
    /// 空片时提示"首烧"，否则提示"升级/修复"。前端据此换文案。
    pub first_flash: bool,
}

/// 烧写结果。`verified` 为 false 时**不允许**当成成功展示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlashReport {
    pub port: String,
    pub from_version: Option<String>,
    pub to_version: String,
    pub bytes_written: u64,
    pub md5: String,
    pub verified: bool,
    pub elapsed_ms: u64,
}

/// 加载好的镜像（含原始字节，不跨 IPC）。
#[derive(Debug, Clone)]
pub struct FirmwareImage {
    pub info: ImageInfo,
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// 镜像解析（纯离线，不需要设备）
// ---------------------------------------------------------------------------

/// 读合并镜像并解析版本。
///
/// 只接受 `idf.py merge-bin` 的产物（从 `0x0` 铺到 app 末尾的连续镜像）。
/// 单独 `ai-passport-mic.bin` 的布局不一样，硬塞进来会算错偏移，所以直接拒绝。
pub fn load_image(path: &Path) -> Result<FirmwareImage> {
    let data = std::fs::read(path)
        .with_context(|| format!("读不到固件镜像 {}", path.display()))
        .map_err(|e| AppError::new(ErrorCode::FirmwareImageInvalid, format!("{e:#}")))?;

    if data.len() <= APP_OFFSET as usize {
        return Err(AppError::new(
            ErrorCode::FirmwareImageInvalid,
            format!(
                "镜像只有 {} 字节，短于 app 分区起点 {:#X}——这不是合并镜像（要用 idf.py merge-bin 的产物）",
                data.len(),
                APP_OFFSET
            ),
        )
        .into());
    }

    let version = read_version(&data)?;
    let md5 = format!("{:032x}", md5_u128(&data[APP_OFFSET as usize..]));

    Ok(FirmwareImage {
        info: ImageInfo {
            path: path.to_path_buf(),
            version,
            bytes: data.len() as u64,
            md5,
        },
        data,
    })
}

/// 从合并镜像里读 `esp_app_desc_t.version`。
fn read_version(data: &[u8]) -> Result<String> {
    let desc = APP_DESC_ADDR as usize;
    if data.len() < desc + VERSION_IN_DESC as usize + VERSION_LEN {
        return Err(AppError::new(
            ErrorCode::FirmwareImageInvalid,
            "镜像长度不够放下 app 描述符".to_string(),
        )
        .into());
    }

    let magic = u32::from_le_bytes(data[desc..desc + 4].try_into().expect("4 字节"));
    if magic != APP_DESC_MAGIC {
        return Err(AppError::new(
            ErrorCode::FirmwareImageInvalid,
            format!(
                "{:#X} 处的 app 描述符 magic 是 {magic:#010X}，期望 {APP_DESC_MAGIC:#010X}——镜像布局不对",
                APP_DESC_ADDR
            ),
        )
        .into());
    }

    let raw = &data[desc + VERSION_IN_DESC as usize..desc + VERSION_IN_DESC as usize + VERSION_LEN];
    let text = raw
        .split(|b| *b == 0)
        .next()
        .unwrap_or_default();
    let version = String::from_utf8_lossy(text).trim().to_string();
    if version.is_empty() {
        return Err(AppError::new(
            ErrorCode::FirmwareImageInvalid,
            "app 描述符里的 version 是空串".to_string(),
        )
        .into());
    }
    Ok(version)
}

/// 核对镜像里的分区表确实是 `nvs@0x9000` + `factory@0x10000`。
///
/// 切片偏移是写死的常量，分区表一变就必须拒绝烧写——否则"跳过 NVS"会变成"写到别处"，
/// 比直接清空更糟。
fn verify_partition_layout(data: &[u8]) -> Result<()> {
    let base = PARTITION_TABLE_OFFSET as usize;
    if data.len() < base + PARTITION_ENTRY_LEN {
        bail!("镜像里没有分区表");
    }

    let magic = u16::from_le_bytes([data[base], data[base + 1]]);
    if magic != PARTITION_MAGIC {
        bail!(
            "分区表 magic 是 {magic:#06X}，期望 {PARTITION_MAGIC:#06X}——{:#X} 处不是分区表",
            PARTITION_TABLE_OFFSET
        );
    }

    let mut nvs_at = None;
    let mut factory_at = None;

    // 第 0 条就是第一个分区。遇到 magic 不对（含末尾的 md5 条目，magic 是 0xEBEB）就停下。
    for i in 0..8 {
        let off = base + i * PARTITION_ENTRY_LEN;
        if off + PARTITION_ENTRY_LEN > data.len() {
            break;
        }
        let e = &data[off..off + PARTITION_ENTRY_LEN];
        let entry_magic = u16::from_le_bytes([e[0], e[1]]);
        if entry_magic != PARTITION_MAGIC {
            break;
        }
        let (ty, sub) = (e[2], e[3]);
        let offset = u32::from_le_bytes(e[4..8].try_into().expect("4 字节"));
        if ty == PARTITION_TYPE_DATA && sub == PARTITION_SUBTYPE_NVS {
            nvs_at = Some(offset);
        }
        if ty == PARTITION_TYPE_APP && sub == PARTITION_SUBTYPE_FACTORY {
            factory_at = Some(offset);
        }
    }

    if nvs_at != Some(NVS_OFFSET) {
        bail!(
            "分区表里 nvs 在 {:X?}，期望 {NVS_OFFSET:#X}；切片常量与镜像不一致，拒绝烧写",
            nvs_at
        );
    }
    if factory_at != Some(APP_OFFSET) {
        bail!(
            "分区表里 factory 在 {:X?}，期望 {APP_OFFSET:#X}；切片常量与镜像不一致，拒绝烧写",
            factory_at
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 设备枚举
// ---------------------------------------------------------------------------

/// 列出疑似目标的串口。**不连接、不复位**，所以可以随便调。
///
/// macOS 上必须去重：`available_ports()` 会把同一个物理口的 callout（`/dev/cu.*`）
/// 和 dialin（`/dev/tty.*`）各返回一条，界面照着渲染就是"插了一台设备、列表里两台"，
/// 而且点 dialin 那条去连必然失败（它等载波，不是给 esptool 用的）。
/// 这里按序列号去重，并优先保留 `cu`。
pub fn list_ports() -> Vec<PortView> {
    let candidates = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| {
            let serialport::SerialPortType::UsbPort(info) = p.port_type else {
                return None;
            };
            if info.vid != TARGET_VID || info.pid != TARGET_PID {
                return None;
            }
            let is_cu = !p.port_name.contains("/tty.") && !p.port_name.starts_with("/dev/tty");
            Some((
                PortView {
                    path: p.port_name,
                    serial_number: info.serial_number,
                },
                is_cu,
            ))
        })
        .collect();
    dedupe_ports(candidates)
}

/// 去重本体：同一个序列号只留一条，`cu` 优先。抽出来是为了能单测
/// （`available_ports()` 依赖真实硬件，没法在测试里造）。
fn dedupe_ports(candidates: Vec<(PortView, bool)>) -> Vec<PortView> {
    let mut out: Vec<(PortView, bool)> = Vec::new(); // (view, is_cu)

    for (view, is_cu) in candidates {
        // 没有序列号的（少见）不去重：宁可多列一条，也不要把两台不同的设备合成一台。
        match out
            .iter_mut()
            .find(|(v, _)| v.serial_number.is_some() && v.serial_number == view.serial_number)
        {
            Some((existing, existing_is_cu)) => {
                if is_cu && !*existing_is_cu {
                    *existing = view;
                    *existing_is_cu = true;
                }
            }
            None => out.push((view, is_cu)),
        }
    }

    out.into_iter().map(|(v, _)| v).collect()
}

/// 检测设备当前固件版本。**会让设备复位**（见模块文档的「副作用」）。
pub fn probe(port: &str, image: &FirmwareImage) -> Result<ProbeResult> {
    // guard 保证无论成功还是读失败，设备最后都会被放回应用模式。
    let mut guard = ResetGuard::new(connect(port)?);
    let flasher = &mut guard.0;
    let chip = flasher.chip();

    // 设备可能已经有固件但描述符坏了，也可能一片空白——都按"未知/空片"处理，不报错。
    let raw = read_desc_bytes(flasher)?;

    let blank = raw.iter().all(|b| *b == 0xFF);
    let current = version_from_desc(&raw);

    let state = if blank {
        DeviceState::Blank
    } else {
        match &current {
            Some(v) if *v == image.info.version => DeviceState::UpToDate,
            Some(_) => DeviceState::Mismatch,
            None => DeviceState::Unknown,
        }
    };

    Ok(ProbeResult {
        port: port.to_string(),
        chip: format!("{chip}"),
        current_version: current,
        image_version: image.info.version.clone(),
        state,
        first_flash: matches!(state, DeviceState::Blank | DeviceState::Unknown),
    })
}

// ---------------------------------------------------------------------------
// 烧写
// ---------------------------------------------------------------------------

/// 烧写并校验。
///
/// `on_progress` 收到 0–100 的整体进度；三段按字节数加权，不是每段 33%。
/// 返回 `Err` 就是没成功——调用方不许把 Err 渲染成"完成，但有警告"。
pub fn flash(
    port: &str,
    image: &FirmwareImage,
    mut on_progress: impl FnMut(u8),
) -> Result<FlashReport> {
    let started = Instant::now();

    verify_partition_layout(&image.data)?;

    let data = &image.data;
    // 三段切片。中间 [NVS_OFFSET, APP_OFFSET) 刻意不出现——那是 nvs 与 phy_init。
    let segments = [
        Segment::new(
            BOOTLOADER_OFFSET,
            &data[BOOTLOADER_OFFSET as usize..PARTITION_TABLE_OFFSET as usize],
        ),
        Segment::new(
            PARTITION_TABLE_OFFSET,
            &data[PARTITION_TABLE_OFFSET as usize..NVS_OFFSET as usize],
        ),
        Segment::new(APP_OFFSET, &data[APP_OFFSET as usize..]),
    ];

    let total_bytes: u64 = segments.iter().map(|s| s.data.len() as u64).sum();

    on_progress(0);

    // 第一段会话：读旧版本 + 写三段。
    // `write_bins_to_flash` 收尾是 `finish(reboot = true)`——写完设备就自己跑起来了，
    // 所以后面想再做任何事都必须重开一次会话。
    let from_version = {
        let mut guard = ResetGuard::new(connect(port)?);
        let flasher = &mut guard.0;
        let from_version = read_installed_version(flasher);

        let mut agg = Progress {
            cb: &mut on_progress,
            done: 0,
            total: total_bytes,
            stage_len: 0,
        };

        // **一次调用写完三段**。别改回"for 循环里一段一次 write_bin_to_flash"：
        // 那个函数内部同样是 begin → 写 → finish(reboot = true)，每段结束都会让芯片
        // 从下载模式复位跑起来，第二段的 SPI attach 必然等不到应答。
        // 2026-11-06 真机第一次点「写入固件」就死在 partition-table 上，就是这条。
        flasher.write_bins_to_flash(&segments, &mut agg).map_err(|e| {
            AppError::new(
                ErrorCode::FlashFailed,
                format!("写 flash 失败: {}", describe(&e)),
            )
        })?;

        from_version
    };

    // 第二段会话：让设备自己算 app 分区的 MD5，与本地比对。
    // 回读 650 KB 要十几秒，FlashMd5 只要一条命令——校验的不是"能不能读"，是"写进去的对不对"。
    // 必须在**新会话**里发：写完那一刻设备已在跑应用，旧连接上发 FlashMd5 没人应答
    //（真机报 `Timeout while running FlashMd5 command`）。ResetGuard 退出时再放回应用模式。
    on_progress(96);
    let app_len = (data.len() - APP_OFFSET as usize) as u32;
    let device_md5 = {
        let mut guard = ResetGuard::new(connect(port)?);
        guard.0.checksum_md5(APP_OFFSET, app_len).map_err(|e| {
            AppError::new(
                ErrorCode::FlashFailed,
                format!("设备端 MD5 计算失败: {}", describe(&e)),
            )
        })?
    };
    let local_md5 = md5_u128(&data[APP_OFFSET as usize..]);

    on_progress(100);

    let verified = device_md5 == local_md5;
    if !verified {
        return Err(AppError::new(
            ErrorCode::FlashVerifyFailed,
            format!(
                "回读校验不一致：设备 {device_md5:032x} / 镜像 {local_md5:032x}。设备可能仍停在下载模式，直接重试即可"
            ),
        )
        .into());
    }

    Ok(FlashReport {
        port: port.to_string(),
        from_version,
        to_version: image.info.version.clone(),
        bytes_written: total_bytes,
        md5: format!("{local_md5:032x}"),
        verified,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// 烧写前顺手记一下设备上原来的版本，失败回滚时用户看得懂"从哪个版本开始坏的"。
/// 读不到不报错——空片本来就该读不到。
fn read_installed_version(flasher: &mut Flasher) -> Option<String> {
    let raw = read_desc_bytes(flasher).ok()?;
    version_from_desc(&raw)
}

/// 从 `read_desc_bytes` 的结果里取版本字符串。magic 不对或空串都返回 None。
fn version_from_desc(raw: &[u8]) -> Option<String> {
    let desc = raw.get(APP_DESC_IN_APP as usize..)?;
    let magic = u32::from_le_bytes(desc.get(0..4)?.try_into().ok()?);
    if magic != APP_DESC_MAGIC {
        return None;
    }
    let text = desc
        .get(VERSION_IN_DESC as usize..)?
        .split(|b| *b == 0)
        .next()?;
    let s = String::from_utf8_lossy(text).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// 从 `APP_OFFSET` 起读 `0x20 + 0x10 + 0x20` 字节（镜像头 + 描述符头 + 版本字段）。
///
/// **必须走 `read_flash`（stub 路径），不能走 `read_flash_rom`。** ROM 那条命令在
/// stub 起起来之后会直接被拒（真机报 `The bootloader returned an error`），
/// 2026-11-06 第一次真机点「检测」就是栽在这上面。`read_flash` 还会让设备自己算一遍
/// 这几十个字节的 MD5，读错能当场发现。
fn read_desc_bytes(flasher: &mut Flasher) -> Result<Vec<u8>> {
    let len = APP_DESC_IN_APP + VERSION_IN_DESC + VERSION_LEN as u32;
    let tmp = temp_probe_path();

    if let Err(e) = flasher.read_flash(
        APP_OFFSET,
        len,
        READ_BLOCK_SIZE,
        READ_MAX_IN_FLIGHT,
        tmp.clone(),
    ) {
        std::fs::remove_file(&tmp).ok();
        return Err(AppError::new(
            ErrorCode::FlashFailed,
            format!("读 {APP_OFFSET:#X} 起 {len} 字节失败: {}", describe(&e)),
        )
        .into());
    }

    let bytes = std::fs::read(&tmp).with_context(|| format!("读临时文件 {} 失败", tmp.display()));
    std::fs::remove_file(&tmp).ok();
    bytes
}

/// 临时文件名带进程号与纳秒时间戳：GUI 与 CLI 同时 probe 时不会互相踩。
fn temp_probe_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("apm-probe-{}-{nanos}.bin", std::process::id()))
}

// ---------------------------------------------------------------------------
// 内部：连接与进度
// ---------------------------------------------------------------------------

fn connect(port: &str) -> Result<Flasher> {
    let info = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .find(|p| p.port_name == port)
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::FlashNoDevice,
                format!("串口 {port} 不在了——设备被拔掉或换了口"),
            )
        })?;

    let usb = match info.port_type {
        serialport::SerialPortType::UsbPort(u) => u,
        _ => {
            return Err(AppError::new(
                ErrorCode::FlashNoDevice,
                format!("{port} 不是 USB 设备，拒绝操作（避免写错串口）"),
            )
            .into())
        }
    };
    if usb.vid != TARGET_VID || usb.pid != TARGET_PID {
        return Err(AppError::new(
            ErrorCode::FlashNoDevice,
            format!(
                "{port} 的 VID:PID 是 {:04X}:{:04X}，不是目标设备的 {TARGET_VID:04X}:{TARGET_PID:04X}",
                usb.vid, usb.pid
            ),
        )
        .into());
    }

    let serial = serialport::new(port, CONNECT_BAUD)
        .timeout(Duration::from_secs(3))
        .open_native()
        .with_context(|| format!("打开串口 {port} 失败（可能被别的程序占用）"))?;

    let conn = Connection::new(
        serial,
        usb,
        ResetAfterOperation::HardReset,
        ResetBeforeOperation::DefaultReset,
        CONNECT_BAUD,
    );

    // `Some(Chip::Esp32c3)` 是硬约束：芯片不对就 ChipMismatch，不靠 VID/PID 猜。
    let flasher = Flasher::connect(conn, true, false, false, Some(Chip::Esp32c3), Some(FLASH_BAUD))
        .map_err(|e| {
            AppError::new(
                ErrorCode::FlashFailed,
                format!("连接芯片失败: {}。确认设备已插好；反复失败可按住 BOOT 再插一次", describe(&e)),
            )
        })?;

    Ok(flasher)
}

/// 把设备放回应用模式的 guard。
///
/// espflash 的 `Flasher` **没有 `Drop`**：进了下载模式就没人负责退出。少这一步，
/// 「点一次检测」的后果是设备停在 ROM 下载模式——不广播、不应答 BLE，用户只能
/// 拔插或长按电源键。所以用 guard 兜住，读失败、写失败、提前 return 都会复位。
struct ResetGuard(Flasher);

impl ResetGuard {
    fn new(flasher: Flasher) -> Self {
        Self(flasher)
    }
}

impl Drop for ResetGuard {
    fn drop(&mut self) {
        // 这里失败只可能是线被拔了；设备本来也会跟着掉电重启，不值得再报一次错。
        let _ = self.0.connection().reset();
    }
}

/// espflash 的 `Error` 只把一句"Communication error while flashing device"写在最外层，
/// 真正的原因（超时？串口 IO？ROM 报错？）在 `source()` 链里。不展开就只能瞎猜。
fn describe(e: &espflash::Error) -> String {
    use std::error::Error as _;
    let mut text = e.to_string();
    let mut src = e.source();
    while let Some(inner) = src {
        text.push_str(&format!(" <- {inner}"));
        src = inner.source();
    }
    text
}

/// 三段共用一个进度累加器，按字节数折算成整体的 0–100。
struct Progress<'a, F: FnMut(u8)> {
    cb: &'a mut F,
    done: u64,
    total: u64,
    stage_len: usize,
}

impl<F: FnMut(u8)> ProgressCallbacks for Progress<'_, F> {
    fn init(&mut self, _addr: u32, total: usize) {
        self.stage_len = total;
    }

    fn update(&mut self, current: usize) {
        if self.total == 0 {
            return;
        }
        let overall = self.done + current as u64;
        let pct = (overall.saturating_mul(100) / self.total).min(100) as u8;
        (self.cb)(pct);
    }

    fn verifying(&mut self) {
        (self.cb)(99);
    }

    fn finish(&mut self, _skipped: bool) {
        self.done += self.stage_len as u64;
    }
}

/// MD5 转 `u128`，与 `espflash` 的 `checksum_md5` 和 `md5sum` 输出对齐（大端读）。
fn md5_u128(data: &[u8]) -> u128 {
    use md5::{Digest, Md5};
    let digest = Md5::digest(data);
    u128::from_be_bytes(digest.into())
}

/// 只做镜像解析，不碰设备。给"随包镜像坏了"这类问题做自检。
pub fn inspect_image(path: &Path) -> Result<ImageInfo> {
    Ok(load_image(path)?.info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个最小可解析的合并镜像：只有三段，中间按 `0xFF` 填。
    fn synthetic_image(version: &str) -> Vec<u8> {
        let app_len = 4096;
        let mut v = vec![0xFFu8; APP_OFFSET as usize + app_len];

        // 分区表：两条有效条目 + 结束符。
        let base = PARTITION_TABLE_OFFSET as usize;
        v[base..base + 2].copy_from_slice(&PARTITION_MAGIC.to_le_bytes());
        v[base + 2] = PARTITION_TYPE_DATA;
        v[base + 3] = PARTITION_SUBTYPE_NVS;
        v[base + 4..base + 8].copy_from_slice(&NVS_OFFSET.to_le_bytes());
        v[base + 8..base + 12].copy_from_slice(&0x6000u32.to_le_bytes());

        let e2 = base + PARTITION_ENTRY_LEN;
        v[e2..e2 + 2].copy_from_slice(&PARTITION_MAGIC.to_le_bytes());
        v[e2 + 2] = PARTITION_TYPE_APP;
        v[e2 + 3] = PARTITION_SUBTYPE_FACTORY;
        v[e2 + 4..e2 + 8].copy_from_slice(&APP_OFFSET.to_le_bytes());
        v[e2 + 8..e2 + 12].copy_from_slice(&0x7F0000u32.to_le_bytes());

        // app 描述符。version 是 NUL 填充的定长数组——真实镜像就是这样，别用 0xFF 造。
        let desc = APP_DESC_ADDR as usize;
        v[desc..desc + 4].copy_from_slice(&APP_DESC_MAGIC.to_le_bytes());
        let vstart = desc + VERSION_IN_DESC as usize;
        v[vstart..vstart + VERSION_LEN].fill(0);
        v[vstart..vstart + version.len()].copy_from_slice(version.as_bytes());

        v
    }

    fn write_temp(name: &str, bytes: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("apm-fw-test-{name}.bin"));
        std::fs::write(&p, bytes).expect("写临时镜像");
        p
    }

    #[test]
    fn loads_version_from_merged_image() {
        let p = write_temp("ok", &synthetic_image("0.1.0"));
        let img = load_image(&p).expect("应能解析");
        assert_eq!(img.info.version, "0.1.0");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn rejects_short_image() {
        let p = write_temp("short", &vec![0u8; 0x100]);
        let err = load_image(&p).expect_err("短镜像必须拒绝");
        let app = err.downcast_ref::<AppError>().expect("应是 AppError");
        assert_eq!(app.code, ErrorCode::FirmwareImageInvalid);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut v = synthetic_image("0.1.0");
        let desc = APP_DESC_ADDR as usize;
        v[desc..desc + 4].copy_from_slice(&0u32.to_le_bytes());
        let p = write_temp("magic", &v);
        let err = load_image(&p).expect_err("magic 不对必须拒绝");
        let app = err.downcast_ref::<AppError>().expect("应是 AppError");
        assert_eq!(app.code, ErrorCode::FirmwareImageInvalid);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn partition_layout_guard_passes_on_real_shape() {
        verify_partition_layout(&synthetic_image("0.1.0")).expect("正常布局应通过");
    }

    #[test]
    fn partition_layout_guard_rejects_moved_nvs() {
        let mut v = synthetic_image("0.1.0");
        let base = PARTITION_TABLE_OFFSET as usize;
        // 把 nvs 挪到 0xA000：切片常量就不再成立。
        v[base + 4..base + 8].copy_from_slice(&0xA000u32.to_le_bytes());
        let err = verify_partition_layout(&v).expect_err("nvs 偏移变了必须拒绝");
        assert!(format!("{err:#}").contains("nvs"), "{err:#}");
    }

    #[test]
    fn md5_matches_known_vector() {
        // `printf '' | md5sum` = d41d8cd98f00b204e9800998ecf8427e
        assert_eq!(md5_u128(b""), 0xd41d8cd98f00b204e9800998ecf8427e);
        // 与 espflash 的打印格式（{checksum:032x}）一致。
        assert_eq!(format!("{:032x}", md5_u128(b"")), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn version_addr_constants_are_self_consistent() {
        assert_eq!(APP_DESC_ADDR, 0x10020);
        assert_eq!(VERSION_ADDR, 0x10030);
    }

    /// 同一台设备的 cu / tty 两条只留一条，且留 cu。
    ///
    /// 这条是回归测试：macOS 上 `available_ports()` 就是这么返回两份的，
    /// 少一次去重，界面就会多出一台点不动的"设备"（2026-11-06 现场）。
    #[test]
    fn dedupes_cu_and_tty_of_the_same_port() {
        let serial = Some("98:C3:77:F6:C6:44".to_string());
        let got = dedupe_ports(vec![
            (
                PortView {
                    path: "/dev/tty.usbmodem11301".into(),
                    serial_number: serial.clone(),
                },
                false,
            ),
            (
                PortView {
                    path: "/dev/cu.usbmodem11301".into(),
                    serial_number: serial.clone(),
                },
                true,
            ),
        ]);
        assert_eq!(got.len(), 1, "两条应合并成一条：{got:?}");
        assert_eq!(got[0].path, "/dev/cu.usbmodem11301", "应保留 callout 节点");
    }

    #[test]
    fn keeps_devices_without_serial_numbers_apart() {
        let got = dedupe_ports(vec![
            (
                PortView {
                    path: "/dev/cu.usbmodem1".into(),
                    serial_number: None,
                },
                true,
            ),
            (
                PortView {
                    path: "/dev/cu.usbmodem2".into(),
                    serial_number: None,
                },
                true,
            ),
        ]);
        assert_eq!(got.len(), 2, "没有序列号就不敢合，宁可多列一条");
    }

    #[test]
    fn keeps_two_different_devices_apart() {
        let got = dedupe_ports(vec![
            (
                PortView {
                    path: "/dev/cu.usbmodem1".into(),
                    serial_number: Some("AA".into()),
                },
                true,
            ),
            (
                PortView {
                    path: "/dev/cu.usbmodem2".into(),
                    serial_number: Some("BB".into()),
                },
                true,
            ),
        ]);
        assert_eq!(got.len(), 2);
    }

    /// 描述符解析：magic 对、版本非空才给 Some。
    #[test]
    fn reads_version_from_desc_slice() {
        let img = synthetic_image("0.1.0");
        let raw = &img[APP_OFFSET as usize..];
        assert_eq!(version_from_desc(raw).as_deref(), Some("0.1.0"));
    }

    #[test]
    fn rejects_desc_slice_with_bad_magic() {
        let mut img = synthetic_image("0.1.0");
        let magic_at = APP_OFFSET as usize + APP_DESC_IN_APP as usize;
        img[magic_at..magic_at + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(version_from_desc(&img[APP_OFFSET as usize..]), None);
    }

    /// 拿真实构建产物验一遍偏移常量。
    ///
    /// 这条测试的价值不在"跑通"，在于**谁改了分区表或镜像布局它就炸**：
    /// 常量是写死的，分区表是仓库控制的，两者必须同时动。没有产物时明说 SKIP，
    /// 不假装通过。
    #[test]
    fn parses_real_build_artifact_if_present() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let merged = root.join("device/build/ai-passport-mic-merged.bin");
        let single = root.join("device/build/ai-passport-mic.bin");

        if !merged.exists() {
            eprintln!("SKIP: 没有 device/build/ai-passport-mic-merged.bin，先跑 ./scripts/build-firmware.sh");
            return;
        }

        let img = load_image(&merged).expect("真实合并镜像应能解析");
        verify_partition_layout(&img.data).expect("真实分区表应通过切片校验");
        assert!(!img.info.version.is_empty(), "版本不该是空串");
        assert!(
            !img.info.version.contains("dirty"),
            "版本里出现 dirty 说明 PROJECT_VER 没固定住：{}",
            img.info.version
        );

        if single.exists() {
            let app = std::fs::read(&single).expect("读 app.bin");
            assert_eq!(
                &img.data[APP_OFFSET as usize..],
                app.as_slice(),
                "合并镜像的 app 段应与单独产物逐字节相同"
            );
        }
    }
}
