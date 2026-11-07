//! btleplug 之上的最小封装。名字与 UUID 都来自 `apm_protocol` 的常量，
//! 不在这里另抄一份 —— 抄一份就会有一天两份不一致。

use anyhow::{anyhow, Context, Result};
use apm_protocol::frame::{self, msg, Event, Status};
use apm_protocol::{
    AUDIO_UUID, BATTERY_LEVEL_UUID, CTRL_UUID, EVENT_UUID, PROFILE_UUID, SERVICE_UUID, STATUS_UUID,
};
use btleplug::api::{
    Central, CentralState, Characteristic, Manager as _, Peripheral as _, ScanFilter,
    ValueNotification, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::stream::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

type NotificationStream = Pin<Box<dyn futures::Stream<Item = ValueNotification> + Send>>;

/// 读一次电量特征值的超时。
///
/// **必须带超时**：设备回 ATT 错误时，macOS 的 btleplug `read()` 会挂住不返回。而这一步
/// 排在 connect 成功之后、`Link` 交回调用方之前，一挂就让整个连接流程停在「连接中」——
/// 设备侧日志显示已连接、已订阅，PC 侧一行不写，极难定位（2026-11-06 现场）。
/// 电量不是关键路径，读不到就降级成"不可用"，不能拖住连接。
const BATTERY_READ_TIMEOUT: Duration = Duration::from_secs(2);

fn uuid(text: &str) -> Uuid {
    Uuid::parse_str(text).expect("常量 UUID 必須合法")
}

pub fn service_uuid() -> Uuid {
    uuid(SERVICE_UUID)
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub rssi: Option<i16>,
    /// 广播或 GATT 里有本项目的服务 UUID。没有也可能是只广播名字的设备。
    pub has_service: bool,
    pub connected: bool,
}

/// 一条音频通知，带**真到达时刻**与**等待时长**。
///
/// `received_at` 是 btleplug 把这条通知交出来的那一刻，在 `next_notification` 里
/// 立刻打点。它存在的唯一理由：让"块到达间隔"与"我们处理得慢不慢"成为两个
/// **独立可测**的量。
///
/// 2026-11-07 之前，到达间隔是在 `Pipeline::on_audio` 里用 `Instant::now()` 量的
/// —— 而那个函数跑在消费循环里，前面还排着 STATUS 心跳的日志落盘。于是量到的
/// "到达间隔"里混进了 PC 自己停顿的时间，用它去证明"链路抖动是根因"是不成立的
/// （见 `docs/KNOWN_ISSUES.md` KI-006 的 2026-11-07 复核）。
///
/// `read_waited` 是**第三把尺子**，用来补掉仍能骗过前两把的那个情形：
/// 如果消费循环在两次读取之间卡住（例如每秒一次的 `println!` 写文件），
/// 数据其实早就躺在队列里，但 `received_at` 依然会被推迟到我们回来才取，
/// 于是"到达间隔"变长、"处理耗时"依然接近 0。有了 `read_waited` 就能分辨：
/// - `read_waited` 也很长 → 数据确实还没到，是链路/主机蓝牙栈晚交的；
/// - `read_waited` ≈ 0 而间隔很长 → **我们问晚了**，是我们自己卡在别处。
#[derive(Debug, Clone)]
pub struct AudioBlock {
    pub data: Vec<u8>,
    /// 通知真正到达 PC 的时刻（btleplug 交出即打点）。
    pub received_at: Instant,
    /// 本次等待花了多久：从开始 await 到通知交出来。
    pub read_waited: Duration,
}

/// 连上之后能收到的东西。
#[derive(Debug, Clone)]
pub enum Notification {
    Audio(AudioBlock),
    Status(Status),
    Event(Event),
    /// PONG 回显的 seq。PONG 与 EVENT 共用特征值，靠 `frame.msg` 区分。
    Pong(u32),
    /// 标准 Battery Service（0x2A19）的电量百分比。旧固件没有该服务时不出现。
    Battery(u8),
    /// **收到了一条通知，但解析不出来**（帧 CRC 错、载荷长度不对、电量值越界、
    /// 或未知特征值）。
    ///
    /// 单独一个变体而不是返回 `Ok(None)`：`Ok(None)` 的语义是"这段时间真的没有通知"，
    /// 而这里恰恰相反 —— 设备发了东西，链路是通的。混成 `Ok(None)` 会让
    /// `monitor_loop` 的"静默 5 s → 探活"把坏帧当成安静，推迟故障发现，
    /// 也让"设备在发但帧坏了"这个现象在日志里完全消失（B6）。
    Malformed { uuid: Uuid, reason: String },
}

/// 连上之后的链路。
///
/// **`Link` 是 `Clone + Send + Sync`**，可以随意 clone 后移进任何 future。
/// 这点是有代价换来的：通知流本身是 `Pin<Box<dyn Stream + Send>>`（`Send` 但不
/// `Sync`），直接放在结构体里会让 `Link: !Sync`，于是 `&Link` 不是 `Send`，
/// `&self` 上的 async 方法就没法在要求 Send 的地方 await（Tauri 命令、`tokio::spawn`）。
/// 因此通知流装进 `Arc<AsyncMutex<…>>`：`Mutex<T>: Sync` 只要 `T: Send`，
/// 这一层包装就把 `Link` 拉回 `Send + Sync`，同时 clone 之间仍共享同一条流。
#[derive(Clone)]
pub struct Link {
    peripheral: Peripheral,
    ctrl: Characteristic,
    #[allow(dead_code)]
    profile: Option<Characteristic>,
    status: Characteristic,
    audio: Characteristic,
    event: Characteristic,
    /// 标准 Battery Level（0x2A19）。旧固件没有这个服务时为 `None`，
    /// 电量读数与通知都静默降级。
    battery: Option<Characteristic>,
    notifications: Arc<AsyncMutex<NotificationStream>>,
}

async fn default_adapter() -> Result<Adapter> {
    let manager = Manager::new().await.context("初始化蓝牙管理器失败")?;
    let adapters = manager.adapters().await.context("枚举蓝牙适配器失败")?;
    adapters
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("没有可用的蓝牙适配器（macOS 上检查系统设置里的蓝牙权限）"))
}

fn describe(
    peripheral: &Peripheral,
    props: Option<&btleplug::api::PeripheralProperties>,
) -> DeviceInfo {
    let name = props.and_then(|p| p.local_name.clone()).unwrap_or_default();
    let has_service = props
        .map(|p| p.services.iter().any(|u| *u == service_uuid()))
        .unwrap_or(false);
    DeviceInfo {
        id: peripheral.id().to_string(),
        name,
        rssi: props.and_then(|p| p.rssi),
        has_service,
        connected: false,
    }
}

/// 只检查有没有**可用**的蓝牙适配器，不做扫描。
///
/// GUI 启动时用它区分 `no_adapter` 与 `idle`，断链后也用它区分
/// "设备没了" 与 "蓝牙被关了"。
///
/// **必须查 `adapter_state()`，不能只看有没有 adapter 对象。**
/// 2026-11-06 真机实测（G1 验收）：macOS 关掉蓝牙后 `manager.adapters()`
/// **仍然返回一个 adapter**，只是扫不到设备。所以旧实现（只取第一个 adapter
/// 就 `Ok`）永远判不出"蓝牙关了"，关蓝牙后状态落到 `idle` 而不是 `no_adapter`，
/// 界面显示「待连接」——用户会以为设备没开机，而真正的原因是蓝牙关了。
///
/// 现在按 `CentralState` 判：`PoweredOn` 才算可用，`PoweredOff` 与 `Unknown`
/// 都报错（`Unknown` 通常出现在刚启动、状态还没同步时，宁可让调用方重试一次）。
pub async fn probe_adapter() -> Result<()> {
    let manager = Manager::new().await.context("初始化蓝牙管理器失败")?;
    let adapters = manager.adapters().await.context("枚举蓝牙适配器失败")?;
    let adapter = adapters
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("没有可用的蓝牙适配器（macOS 上检查系统设置里的蓝牙权限）"))?;

    match adapter
        .adapter_state()
        .await
        .context("读取蓝牙适配器状态失败")?
    {
        CentralState::PoweredOn => Ok(()),
        CentralState::PoweredOff => Err(anyhow!("蓝牙已关闭：请在菜单栏或系统设置里打开蓝牙")),
        CentralState::Unknown => Err(anyhow!(
            "蓝牙状态未知（刚启动或尚未授权）：检查系统设置 → 隐私与安全性 → 蓝牙"
        )),
    }
}

/// 扫描 `timeout`，**边扫边回调**当前快照，`cancel` 置真时提前收尾。
///
/// 与 [`scan`] 的区别：`scan` 是"睡满再一次性返回"，适合 CLI；GUI 需要列表随
/// 时间增长，也需要用户点"停止"能立刻响应。两者共用同一套过滤规则。
///
/// `on_update` 只在快照发生变化时被调用（含首次），避免让前端空转。
pub async fn scan_stream<F>(
    timeout: Duration,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    mut on_update: F,
) -> Result<Vec<DeviceInfo>>
where
    F: FnMut(&[DeviceInfo]),
{
    let adapter = default_adapter().await?;
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("启动扫描失败（macOS 检查蓝牙权限）")?;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut last: Vec<DeviceInfo> = Vec::new();
    let mut emitted = false;

    loop {
        if *cancel.borrow() || tokio::time::Instant::now() >= deadline {
            break;
        }

        let peripherals = adapter.peripherals().await.context("枚举设备失败")?;
        let mut found = Vec::new();
        for peripheral in peripherals {
            let props = peripheral.properties().await.ok().flatten();
            let info = describe(&peripheral, props.as_ref());
            if info.name.is_empty() && !info.has_service {
                continue;
            }
            let connected = peripheral.is_connected().await.unwrap_or(false);
            found.push(DeviceInfo { connected, ..info });
        }
        found.sort_by(|a, b| b.rssi.unwrap_or(i16::MIN).cmp(&a.rssi.unwrap_or(i16::MIN)));

        // 只在变化时回调：RSSI 抖动会让"变化"过频，所以比较 id 集合而不是整个结构。
        let changed = !emitted
            || found.len() != last.len()
            || found
                .iter()
                .zip(&last)
                .any(|(a, b)| a.id != b.id || a.rssi != b.rssi);
        if changed {
            on_update(&found);
            emitted = true;
            last = found.clone();
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
            _ = cancel.changed() => break,
        }
    }

    let _ = adapter.stop_scan().await;
    Ok(last)
}

/// 扫描 `timeout` 秒，返回所有有名字或带本项目服务 UUID 的设备。
pub async fn scan(timeout: Duration) -> Result<Vec<DeviceInfo>> {
    let adapter = default_adapter().await?;
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("启动扫描失败（macOS 检查蓝牙权限）")?;

    tokio::time::sleep(timeout).await;

    let peripherals = adapter.peripherals().await.context("枚举设备失败")?;
    let mut found = Vec::new();
    for peripheral in peripherals {
        let props = peripheral.properties().await.ok().flatten();
        let info = describe(&peripheral, props.as_ref());
        if info.name.is_empty() && !info.has_service {
            continue; // 匿名设备对用户没意义
        }
        let connected = peripheral.is_connected().await.unwrap_or(false);
        found.push(DeviceInfo { connected, ..info });
    }

    let _ = adapter.stop_scan().await;
    found.sort_by(|a, b| b.rssi.unwrap_or(i16::MIN).cmp(&a.rssi.unwrap_or(i16::MIN)));
    Ok(found)
}

/// `pick_chars` 的返回：CTRL / PROFILE / STATUS / AUDIO / EVENT / Battery（均按
/// `connect` 里解构的顺序）。PROFILE 与 Battery 可缺省，其余缺了就连不上。
type PickedChars = (
    Characteristic,
    Option<Characteristic>,
    Characteristic,
    Characteristic,
    Characteristic,
    Option<Characteristic>,
);

fn pick_chars(chars: &std::collections::BTreeSet<Characteristic>) -> Result<PickedChars> {
    let find = |text: &str| -> Option<Characteristic> {
        let target = uuid(text);
        chars.iter().find(|c| c.uuid == target).cloned()
    };

    let ctrl = find(CTRL_UUID).ok_or_else(|| anyhow!("设备没有 CTRL 特征值"))?;
    let profile = find(PROFILE_UUID);
    let status = find(STATUS_UUID).ok_or_else(|| anyhow!("设备没有 STATUS 特征值"))?;
    let audio = find(AUDIO_UUID).ok_or_else(|| anyhow!("设备没有 AUDIO 特征值"))?;
    let event = find(EVENT_UUID).ok_or_else(|| anyhow!("设备没有 EVENT 特征值"))?;
    let battery = find(BATTERY_LEVEL_UUID);

    Ok((ctrl, profile, status, audio, event, battery))
}

/// 连接设备。`selector` 可以是名字子串或 PeripheralId；为 `None` 时选第一个带服务 UUID 的设备。
pub async fn connect(selector: Option<&str>, timeout: Duration) -> Result<Link> {
    let adapter = default_adapter().await?;
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("启动扫描失败")?;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut chosen: Option<Peripheral> = None;
    while tokio::time::Instant::now() < deadline {
        for peripheral in adapter.peripherals().await.context("枚举设备失败")? {
            let props = peripheral.properties().await.ok().flatten();
            let info = describe(&peripheral, props.as_ref());

            let matched = match selector {
                Some(want) => {
                    info.name.to_lowercase().contains(&want.to_lowercase())
                        || info.id.contains(want)
                }
                None => info.has_service,
            };
            if matched {
                chosen = Some(peripheral);
                break;
            }
        }
        if chosen.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let peripheral = chosen.ok_or_else(|| match selector {
        Some(want) => anyhow!("没找到匹配 \"{want}\" 的设备"),
        None => anyhow!("没找到广播 {} 的设备，确认固件已启动", SERVICE_UUID),
    })?;

    let _ = adapter.stop_scan().await;

    peripheral.connect().await.context("连接失败")?;
    peripheral
        .discover_services()
        .await
        .context("发现 GATT 服务失败")?;

    let chars = peripheral.characteristics();
    let (ctrl, profile, status, audio, event, battery) = pick_chars(&chars)?;

    for chr in [&status, &audio, &event] {
        peripheral
            .subscribe(chr)
            .await
            .with_context(|| format!("订阅 {} 失败", chr.uuid))?;
    }
    // 电量通知可有可无：订阅失败不影响连接（旧固件没有这个服务）。
    if let Some(chr) = &battery {
        let _ = peripheral.subscribe(chr).await;
    }

    let notifications = peripheral.notifications().await.context("打开通知流失败")?;

    Ok(Link {
        peripheral,
        ctrl,
        profile,
        status,
        audio,
        event,
        battery,
        notifications: Arc::new(AsyncMutex::new(Box::pin(notifications))),
    })
}

impl Link {
    /// 这只说明句柄还存在，**不**代表对端在线。
    pub fn is_connected(&self) -> bool {
        true
    }

    /// 问平台要一次真实的连接状态（CoreBluetooth 的 `isConnected`）。
    ///
    /// 通知流静默之后必须再问一次链路，否则"设备被拔电"会被当成"设备很安静"，
    /// 界面会一直停在 ready/streaming。
    pub async fn is_alive(&self) -> bool {
        self.peripheral.is_connected().await.unwrap_or(false)
    }

    /// 写 CTRL 帧。CTRL 带 WRITE_NO_RSP，所以默认不等待响应。
    pub async fn send_ctrl(&self, msg_type: u8, payload: &[u8]) -> Result<()> {
        let bytes =
            frame::encode(msg_type, payload).map_err(|e| anyhow!("编码 CTRL 帧失败: {e}"))?;
        self.peripheral
            .write(&self.ctrl, &bytes, WriteType::WithoutResponse)
            .await
            .with_context(|| format!("写 CTRL 失败（{}）", msg::name(msg_type)))
    }

    pub async fn start_capture(&self) -> Result<()> {
        self.send_ctrl(msg::START, &[]).await
    }

    pub async fn stop_capture(&self) -> Result<()> {
        self.send_ctrl(msg::STOP, &[]).await
    }

    pub async fn set_gain(&self, db: i8) -> Result<()> {
        self.send_ctrl(msg::SET_GAIN, &[db as u8]).await
    }

    /// 读一次 STATUS 特征值（也可能从通知里来）。
    pub async fn read_status(&self) -> Result<Status> {
        let value = self
            .peripheral
            .read(&self.status)
            .await
            .context("读 STATUS 失败")?;
        let (frame, _) = frame::decode(&value).map_err(|e| anyhow!("STATUS 帧解析失败: {e}"))?;
        Status::decode(frame.payload).ok_or_else(|| anyhow!("STATUS 载荷长度不足"))
    }

    /// 读一次电量（标准 Battery Service 0x2A19）。
    ///
    /// 旧固件没有该服务时返回 `Ok(None)`；读到的值 > 100 也按"不可用"算——
    /// 设备侧在电量计缺失时保持 0xFF 占位，不会误报 0%。**读超时同样是 `Ok(None)`**，
    /// 不能把"电量读不到"升级成"连接失败"（见 [`BATTERY_READ_TIMEOUT`]）。
    pub async fn read_battery(&self) -> Result<Option<u8>> {
        let Some(chr) = &self.battery else {
            return Ok(None);
        };
        let read = tokio::time::timeout(BATTERY_READ_TIMEOUT, self.peripheral.read(chr)).await;
        let value = match read {
            Ok(Ok(v)) => v,
            Ok(Err(err)) => return Err(anyhow::Error::from(err).context("读 Battery Level 失败")),
            // 超时：设备没回或 ATT 错误把 read 挂住了。电量降级为"不可用"，连接继续。
            Err(_) => return Ok(None),
        };
        let Some(&pct) = value.first() else {
            return Ok(None);
        };
        Ok((pct <= 100).then_some(pct))
    }

    /// 取下一个通知。
    ///
    /// 返回值有三种语义，**不能混为一谈**：
    /// - `Ok(Some(_))`：收到一条能解析的通知（含 AUDIO / STATUS / EVENT / PONG / 电量）；
    /// - `Ok(None)`：**这段时间真的没有通知**（超时）。调用方据此判断"设备安静了"；
    /// - `Err(_)`：通知流结束（设备断开）—— 明确是故障。
    ///
    /// 帧解析失败 / 载荷长度不对 / 电量值越界走 `Err` 的**第四种**：
    /// `Ok(Some(Notification::Malformed { .. }))`。这不是"安静"——设备明明发了东西，
    /// 它恰恰证明链路是通的。旧实现把它返回成 `Ok(None)`，于是 `monitor_loop` 的
    /// "静默超过 5 s 就探活"会把一条 CRC 错的 STATUS 当成"什么都没收到"，
    /// 既推迟探活，也让日志里少了一条本该有的"收到坏帧"证据。
    ///
    /// 只锁通知流本身（不是整个 `Link`），所以 CTRL 写入不会被通知消费挡住；
    /// 反过来同一时刻也只有一个消费者能读流，语义与原来的 `&mut self` 一致。
    pub async fn next_notification(&self, timeout: Duration) -> Result<Option<Notification>> {
        let mut stream = self.notifications.lock().await;
        let asked = Instant::now();
        let next = tokio::time::timeout(timeout, stream.next()).await;
        // 打点必须紧跟 `next()` 返回：这里是"通知到达 PC"的最早可观测时刻。
        // 之后无论调用方排队多久才处理，`received_at` 都不会被那段等待污染。
        let received_at = Instant::now();
        let read_waited = received_at.saturating_duration_since(asked);
        let Some(item) = (match next {
            Ok(v) => v,
            Err(_) => return Ok(None),
        }) else {
            return Err(anyhow!("通知流已结束（设备可能已断开）"));
        };

        if item.uuid == self.audio.uuid {
            return Ok(Some(Notification::Audio(AudioBlock {
                data: item.value,
                received_at,
                read_waited,
            })));
        }
        // 控制面/状态面统一走纯函数解析：抽出来是为了能单测"坏帧必须变成
        // `Malformed` 而不是 `Ok(None)`"（B6）—— 内联在这里就没法测。
        let parsed = classify_frame_notification(
            &item.uuid,
            &item.value,
            self.status.uuid,
            self.event.uuid,
            self.battery.as_ref().map(|c| c.uuid),
        );
        Ok(parsed)
    }

    /// 发一次 PING 并等**匹配 seq** 的 PONG，返回往返时延。
    ///
    /// - 超时返回 `Ok(None)`，链路断开返回 `Err`。
    /// - 期间到达的 STATUS / EVENT / AUDIO 通知会被跳过（不计入时延，也不丢给调用方）：
    ///   `apmctl ping` 不录音，这里丢掉的只可能是 1 s 心跳。
    /// - 测到的是**链路往返**（含连接间隔与设备任务调度），音频单程 ≈ RTT/2，
    ///   不等于端到端声学延迟。
    pub async fn ping(&self, seq: u32, timeout: Duration) -> Result<Option<Duration>> {
        let start = std::time::Instant::now();
        self.send_ctrl(msg::PING, &seq.to_le_bytes()).await?;
        let deadline = start + timeout;

        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            match self.next_notification(deadline - now).await? {
                Some(Notification::Pong(got)) if got == seq => return Ok(Some(start.elapsed())),
                Some(_) => continue,
                // 通知流在这一轮窗口里安静地没东西：交给上面的 deadline 判断。
                None => continue,
            }
        }
    }

    pub async fn disconnect(self) -> Result<()> {
        for chr in [&self.status, &self.audio, &self.event] {
            let _ = self.peripheral.unsubscribe(chr).await;
        }
        if let Some(chr) = &self.battery {
            let _ = self.peripheral.unsubscribe(chr).await;
        }
        self.peripheral.disconnect().await.context("断开失败")
    }
}

/// 把一条（非音频的）通知解析成 [`Notification`]，**永不返回 `None`**。
///
/// 这是 B6 的核心口径：只要收到了通知，返回值就必须是 `Some(_)`。
/// 解析失败的进 `Malformed`，绝不退化成"这段时间没有通知"。
///
/// 抽成纯函数（不碰 `&self`、不 await）是为了能被单测覆盖：
/// 真实外设造不出"CRC 错的 STATUS 通知"，而在生产环境里恰恰是这种帧
/// 决定了 `monitor_loop` 会不会误判设备消失。
fn classify_frame_notification(
    uuid: &Uuid,
    value: &[u8],
    status_uuid: Uuid,
    event_uuid: Uuid,
    battery_uuid: Option<Uuid>,
) -> Option<Notification> {
    let malformed = |reason: String| {
        Some(Notification::Malformed {
            uuid: *uuid,
            reason,
        })
    };

    if Some(*uuid) == battery_uuid {
        return match value.first() {
            Some(&pct) if pct <= 100 => Some(Notification::Battery(pct)),
            Some(&pct) => malformed(format!("Battery Level 值越界（{pct}）")),
            None => malformed("Battery Level 载荷为空".to_string()),
        };
    }

    if *uuid == status_uuid {
        return match frame::decode(value) {
            Ok((f, _)) => match Status::decode(f.payload) {
                Some(status) => Some(Notification::Status(status)),
                None => malformed(format!("STATUS 载荷 {} 字节，不足 8", f.payload.len())),
            },
            Err(e) => malformed(format!("STATUS 帧解析失败：{e}")),
        };
    }

    if *uuid == event_uuid {
        // EVENT 与 PONG 共用这个特征值，必须按 msg 分派，不能按特征值猜类型。
        let Ok((f, _)) = frame::decode(value) else {
            return malformed("EVENT 帧解析失败".to_string());
        };
        return match f.msg {
            msg::EVENT => match Event::decode(f.payload) {
                Some(event) => Some(Notification::Event(event)),
                None => malformed(format!("EVENT 载荷 {} 字节，不足 3", f.payload.len())),
            },
            msg::PONG => match pong_seq(f.payload) {
                Some(seq) => Some(Notification::Pong(seq)),
                None => malformed(format!("PONG 载荷 {} 字节，必须是 4", f.payload.len())),
            },
            other => malformed(format!("EVENT 特征值上出现未知 msg=0x{other:02X}")),
        };
    }

    // 未知特征值的通知：不是错误，但也不该被当成"安静"（说明有别的客户端在写）。
    malformed("未订阅的特征值送来通知".to_string())
}

/// PONG 载荷就是 PING 的 4 字节 seq（小端）。长度不对返回 `None` ——
/// 与设备侧 `apm_build_pong` 的要求一致，两端都不做任意长度透传。
fn pong_seq(payload: &[u8]) -> Option<u32> {
    if payload.len() != 4 {
        return None;
    }
    Some(u32::from_le_bytes([
        payload[0], payload[1], payload[2], payload[3],
    ]))
}

/// 连接时分不清"设备没起来"和"权限没给"，这里统一给出可执行的提示。
pub fn hint_for_error(err: &anyhow::Error) -> &'static str {
    let text = format!("{err:#}");
    if text.contains("permission") || text.contains("Permission") || text.contains("denied") {
        "macOS：系统设置 → 隐私与安全性 → 蓝牙，给终端/IDE 授权后重启进程"
    } else if text.contains("adapter") || text.contains("适配器") {
        "确认蓝牙已打开；虚拟机或远程会话里通常没有可用适配器"
    } else {
        "先跑 `apmctl scan` 确认设备在广播；设备开机后 3 秒内才会广播"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Link` 必须一直是 `Send + Sync + Clone`。
    ///
    /// 这条不是形式主义：通知流本身 `!Sync`，把它直接放进结构体就会让 `&Link`
    /// 不能跨 await，Tauri 命令和 `tokio::spawn` 会一起编译失败（历史上就是这样，
    /// 当时靠复制一个 `Ctrl` 句柄绕过去的）。谁要是把 `Arc<AsyncMutex<…>>` 拆掉，
    /// 这条会立刻红。
    #[test]
    fn link_is_send_sync_and_clone() {
        fn assert_all<T: Send + Sync + Clone>() {}
        assert_all::<Link>();
    }

    /// 编译期回归门：`&Link` 必须能进一个 `Send` 的 future。
    ///
    /// 只构造不运行 —— 能构造出来就说明借用检查与 auto-trait 都过了。
    #[test]
    fn shared_link_can_cross_await_boundary() {
        fn assert_send_future<F: std::future::Future + Send>(f: F) -> F {
            f
        }
        fn build(link: &Link) {
            let fut = assert_send_future(async move {
                let _ = link.is_alive().await;
                let _ = link.next_notification(Duration::from_millis(1)).await;
                let _ = link.start_capture().await;
            });
            drop(fut);
        }
        let _ = build as fn(&Link);
    }

    #[test]
    fn uuids_are_valid() {
        use apm_protocol::BATTERY_SERVICE_UUID;
        for text in [
            SERVICE_UUID,
            CTRL_UUID,
            PROFILE_UUID,
            STATUS_UUID,
            EVENT_UUID,
            AUDIO_UUID,
            BATTERY_SERVICE_UUID,
            BATTERY_LEVEL_UUID,
        ] {
            assert!(Uuid::parse_str(text).is_ok(), "{text} 不是合法 UUID");
        }
        assert_eq!(service_uuid(), Uuid::parse_str(SERVICE_UUID).unwrap());
    }

    #[test]
    fn pong_payload_must_be_four_bytes() {
        assert_eq!(pong_seq(&[0x78, 0x56, 0x34, 0x12]), Some(0x1234_5678));
        assert_eq!(pong_seq(&[]), None);
        assert_eq!(pong_seq(&[1, 2, 3]), None);
        assert_eq!(pong_seq(&[1, 2, 3, 4, 5]), None);
    }

    /// **B6 的核心口径：坏帧不能变成"这段时间没有通知"。**
    ///
    /// `monitor_loop` 靠 `Ok(None)` 判断"设备安静了，该探活了"。如果 CRC 错的
    /// STATUS 也返回 `Ok(None)`，设备明明在发却被当成消失，探活会推迟、
    /// 日志里也少一条"收到坏帧"的证据。
    #[test]
    fn malformed_frames_are_never_reported_as_silence() {
        let status = Uuid::parse_str(STATUS_UUID).unwrap();
        let event = Uuid::parse_str(EVENT_UUID).unwrap();
        let battery = Uuid::parse_str(BATTERY_LEVEL_UUID).unwrap();
        let parse = |uuid: Uuid, value: &[u8]| {
            classify_frame_notification(&uuid, value, status, event, Some(battery))
        };

        // ① CRC 错的 STATUS：改一个字节即可（帧层会报 CRC）。
        let mut bad_status = frame::encode(msg::STATUS, &[0u8; 8]).unwrap();
        bad_status[3] ^= 0xFF;
        match parse(status, &bad_status) {
            Some(Notification::Malformed { reason, .. }) => {
                assert!(reason.contains("STATUS"), "{reason}");
            }
            other => panic!("坏 STATUS 帧必须报 Malformed，实得 {other:?}"),
        }

        // ② 载荷太短的 STATUS（帧本身合法）。
        let short = frame::encode(msg::STATUS, &[0u8; 3]).unwrap();
        assert!(matches!(
            parse(status, &short),
            Some(Notification::Malformed { .. })
        ));

        // ③ 载荷不是 4 字节的 PONG：设备侧也拒绝，这里必须报出来。
        let bad_pong = frame::encode(msg::PONG, &[1, 2, 3]).unwrap();
        assert!(matches!(
            parse(event, &bad_pong),
            Some(Notification::Malformed { .. })
        ));

        // ④ 电量值越界（>100）。
        assert!(matches!(
            parse(battery, &[0xFF]),
            Some(Notification::Malformed { .. })
        ));

        // ⑤ 未知特征值：不是安静，是"别人在写"。
        assert!(matches!(
            parse(Uuid::from_u128(0xdead_beef), &[1, 2, 3]),
            Some(Notification::Malformed { .. })
        ));

        // 反例：合法帧必须正常解析（别把保护写成"一律报错"）。
        let ok_status = frame::encode(msg::STATUS, &[1, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(matches!(
            parse(status, &ok_status),
            Some(Notification::Status(_))
        ));
        let ok_pong = frame::encode(msg::PONG, &[1, 2, 3, 4]).unwrap();
        match parse(event, &ok_pong) {
            Some(Notification::Pong(seq)) => assert_eq!(seq, 0x0403_0201),
            other => panic!("合法 PONG 应当解析成 seq，实得 {other:?}"),
        }
        assert!(matches!(
            parse(battery, &[87]),
            Some(Notification::Battery(87))
        ));
    }

    #[test]
    fn ping_and_pong_are_distinguishable_frames() {
        // 设备只回 4 字节：帧层必须能区分 PING / PONG / EVENT 三种 type。
        let seq = 0x1234_5678u32;
        let bytes = frame::encode(msg::PING, &seq.to_le_bytes()).unwrap();
        let (f, _) = frame::decode(&bytes).unwrap();
        assert_eq!(f.msg, msg::PING);
        assert_ne!(msg::PING, msg::EVENT);
        assert_ne!(msg::PING, msg::PONG);
        assert_eq!(pong_seq(f.payload), Some(seq));
    }
}
