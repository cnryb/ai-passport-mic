//! btleplug 之上的最小封装。名字与 UUID 都来自 `apm_protocol` 的常量，
//! 不在这里另抄一份 —— 抄一份就会有一天两份不一致。

use anyhow::{anyhow, Context, Result};
use apm_protocol::frame::{self, msg, Event, Status};
use apm_protocol::{
    AUDIO_UUID, CTRL_UUID, EVENT_UUID, PROFILE_UUID, SERVICE_UUID, STATUS_UUID,
};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::stream::StreamExt;
use std::pin::Pin;
use std::time::Duration;
use uuid::Uuid;

type NotificationStream = Pin<Box<dyn futures::Stream<Item = ValueNotification> + Send>>;

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

/// 连上之后能收到的东西。
#[derive(Debug, Clone)]
pub enum Notification {
    Audio(Vec<u8>),
    Status(Status),
    Event(Event),
}

pub struct Link {
    peripheral: Peripheral,
    ctrl: Characteristic,
    #[allow(dead_code)]
    profile: Option<Characteristic>,
    status: Characteristic,
    audio: Characteristic,
    event: Characteristic,
    notifications: NotificationStream,
}

async fn default_adapter() -> Result<Adapter> {
    let manager = Manager::new().await.context("初始化蓝牙管理器失败")?;
    let adapters = manager.adapters().await.context("枚举蓝牙适配器失败")?;
    adapters
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("没有可用的蓝牙适配器（macOS 上检查系统设置里的蓝牙权限）"))
}

fn describe(peripheral: &Peripheral, props: Option<&btleplug::api::PeripheralProperties>) -> DeviceInfo {
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

/// 只检查有没有可用的蓝牙适配器，不做扫描。
///
/// GUI 启动时用它区分 `no_adapter` 与 `idle`：macOS 上蓝牙未授权时，
/// 这里通常表现为枚举不到适配器（btleplug 拿不到 CBCentralManager 状态）。
pub async fn probe_adapter() -> Result<()> {
    default_adapter().await.map(|_| ())
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
            || found.iter().zip(&last).any(|(a, b)| a.id != b.id || a.rssi != b.rssi);
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

fn pick_chars(
    chars: &std::collections::BTreeSet<Characteristic>,
) -> Result<(Characteristic, Option<Characteristic>, Characteristic, Characteristic, Characteristic)>
{
    let find = |text: &str| -> Option<Characteristic> {
        let target = uuid(text);
        chars.iter().find(|c| c.uuid == target).cloned()
    };

    let ctrl = find(CTRL_UUID).ok_or_else(|| anyhow!("设备没有 CTRL 特征值"))?;
    let profile = find(PROFILE_UUID);
    let status = find(STATUS_UUID).ok_or_else(|| anyhow!("设备没有 STATUS 特征值"))?;
    let audio = find(AUDIO_UUID).ok_or_else(|| anyhow!("设备没有 AUDIO 特征值"))?;
    let event = find(EVENT_UUID).ok_or_else(|| anyhow!("设备没有 EVENT 特征值"))?;

    Ok((ctrl, profile, status, audio, event))
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
                    info.name.to_lowercase().contains(&want.to_lowercase()) || info.id.contains(want)
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
    let (ctrl, profile, status, audio, event) = pick_chars(&chars)?;

    for chr in [&status, &audio, &event] {
        peripheral
            .subscribe(chr)
            .await
            .with_context(|| format!("订阅 {} 失败", chr.uuid))?;
    }

    let notifications = peripheral
        .notifications()
        .await
        .context("打开通知流失败")?;

    Ok(Link {
        peripheral,
        ctrl,
        profile,
        status,
        audio,
        event,
        notifications: Box::pin(notifications),
    })
}

impl Link {
    pub fn is_connected(&self) -> bool {
        true
    }

    /// 写 CTRL 帧。CTRL 带 WRITE_NO_RSP，所以默认不等待响应。
    pub async fn send_ctrl(&self, msg_type: u8, payload: &[u8]) -> Result<()> {
        let bytes = frame::encode(msg_type, payload).map_err(|e| anyhow!("编码 CTRL 帧失败: {e}"))?;
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

    /// 取下一个通知。超时返回 `Ok(None)`，链路断了返回 `Err`。
    pub async fn next_notification(&mut self, timeout: Duration) -> Result<Option<Notification>> {
        let next = tokio::time::timeout(timeout, self.notifications.next()).await;
        let Some(item) = (match next {
            Ok(v) => v,
            Err(_) => return Ok(None),
        }) else {
            return Err(anyhow!("通知流已结束（设备可能已断开）"));
        };

        if item.uuid == self.audio.uuid {
            return Ok(Some(Notification::Audio(item.value)));
        }
        if item.uuid == self.status.uuid {
            if let Ok((f, _)) = frame::decode(&item.value) {
                if let Some(status) = Status::decode(f.payload) {
                    return Ok(Some(Notification::Status(status)));
                }
            }
            return Ok(None);
        }
        if item.uuid == self.event.uuid {
            if let Ok((f, _)) = frame::decode(&item.value) {
                if let Some(event) = Event::decode(f.payload) {
                    return Ok(Some(Notification::Event(event)));
                }
            }
            return Ok(None);
        }
        Ok(None)
    }

    pub async fn disconnect(self) -> Result<()> {
        for chr in [&self.status, &self.audio, &self.event] {
            let _ = self.peripheral.unsubscribe(chr).await;
        }
        self.peripheral.disconnect().await.context("断开失败")
    }
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

    #[test]
    fn uuids_are_valid() {
        for text in [SERVICE_UUID, CTRL_UUID, PROFILE_UUID, STATUS_UUID, EVENT_UUID, AUDIO_UUID] {
            assert!(Uuid::parse_str(text).is_ok(), "{text} 不是合法 UUID");
        }
        assert_eq!(service_uuid(), Uuid::parse_str(SERVICE_UUID).unwrap());
    }
}
