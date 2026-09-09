# keypoint-rmk-dongle

KeyPoint 分体键盘的 **dongle 拓扑**固件（RMK / nRF52840）。
是 [keypoint-rmk](https://github.com/Iemooon/keypoint-rmk) 的改造版：把"左半兼任中心"
改成独立的 USB 接收器做中心，两个半键盘都作为 BLE split peripheral 无线接入。
运行逻辑对齐已成熟部署的 ZMK 版本（keypoint-zmk-dongle），结构参考
rmk 官方示例 `examples/use_rust/nrf52840_ble_split_dongle`。

## 拓扑

主机侧设备名统一为 **KeyPoint-DG**（USB 产品名 / BLE 广播名 / Vial 键盘名），与有线中心版 keypoint-rmk 的 "KeyPoint" 相区别。

```
        USB                 BLE(2M, 2 links)          BLE(2M)
 host ├────── dongle ──────────────┬──────────── left  (id 0, 矩阵行0..5 + 触摸板 + 左屏)
                                   └───────────── right (id 1, 矩阵行6..11 +指点杆 + 右屏)
```

| 角色 | 文件 | 内容 |
|------|------|------|
| dongle（接收器） | `src/central.rs` | 无键阵/无屏。USB HID 主机 + split BLE central；keymap、Vial、存储、鼠标/滚轮/速度档位/自动鼠标层全部处理逻辑都在这侧。**硬件为 Nordic 官方 nRF52840 Dongle (PCA10059)** |
| left（左半） | `src/left.rs` | 6×8 矩阵（row_offset 0）、A320 触摸板、左 LPM 屏、旋钮 0、电池 ADC、状态 LED；硬件与原 central 半完全一致 |
| right（右半） | `src/right.rs` | 6×8 矩阵（row_offset 6）、TrackPoint、右 LPM 屏、旋钮 1、电池、LED；与原 peripheral 二进制唯一差异是 peripheral id 0→1 |

单 crate、三个 bin（central / left / right），构建任务只换链接模板：接收器与半键盘的应用
基址**都是 0x1000**（2026-09-09 用可用的 ZMK receiver hex 逐字节破案——PCA10059 出厂
bootloader 直接跳 0x1000，早先抄 Zephyr 板配置的 0x10000 是全部"刷写成功但无反应"的根因），
两份模板只差 FLASH 上界：接收器到 0xA0000 为止（正好在 rmk 存储区前收尾，溢出直接变链接
错误），半键盘按 Adafruit UF2 布局到 0xFFFFC。

要点：
- 半键盘上电以 id 0/1 广播，dongle 按 id 落槽（rmk 的 `BleTransport` 矩阵配置数组按
  peripheral id 位置绑定），左右不会串位；`keyboard.toml` 里 `split_peripherals_num = 2`。
- 屏显状态（layer/WPM/修饰键/CapsLock/主机连接状态）由 dongle 经 split 链路转发
  （`SplitMessage::Layer/Wpm/Modifier/ConnectionStatus/...`），两个半键盘各自本地
  `DisplayProcessor` 渲染，沿用原 right 半已验证的路径。
- 触摸板/指点杆的 PointingEvent 带 device id（0=pad, 1=nub）跨链路上传，只有 dongle 侧
  的 `PointingProcessor` 转成 HID 鼠标报告——与原工程同一规则，只是现在 pad 也从
  "本地"变成了"跨链路"。
- dongle 同时保留 BLE HID peripheral 角色：接收器也能无线连主机（USB 插入时自动切 USB）。
- 三个角色共用一份 `keyboard.toml`/事件配额，配额取三角色并集；`clear_peer` 由 1 提到 2
  （dongle 上有两个 PeripheralManager 订阅者）。

## 构建

前置：`rustup target add thumbv7em-none-eabihf`、`cargo install cargo-make`。

```shell
# 接收器：产出 rmk-central.hex
cargo make dongle

# 半键盘：产出 rmk-left.uf2 + rmk-right.uf2
cargo make uf2
```

单编某个角色：`cargo build --release --bin central|left|right`。
接收器 bring-up 隔离开关（默认全关）：`--features softvd`（软件 VBUS 检测）、
`--features bare`（跳过 SDC/BLE 栈只留 USB）。启动阶段由双 LED 报告：panic 时红灯快闪
N 下=挂死阶段号，正常则每 3 s 单闪一次心跳。

## 刷写与配对

- **接收器（官方 nRF52840 Dongle）**：按住板尾按键后插 USB（红灯呼吸=进 bootloader），用
  nRF Connect for Desktop 的 Programmer 写入 `rmk-central.hex`（hex 自带 0x1000 地址，
  Programmer 按地址落页直接 Program 即可，不要 Erase 整片，保留出厂 bootloader）。
  不再需要 nrfutil/DFU zip——那条路已于 2026-09-09 退役。
- **半键盘**：双击复位进 UF2 盘，拖入 `rmk-left.uf2` / `rmk-right.uf2`。
- 顺序建议：先刷两个半键盘，再刷 dongle。dongle 上电后无已存地址时会发起扫描，收编广播中
  带 id 的两个半键盘并完成绑定；此后按存储地址回连。
- 重配对：半键盘上按住 profile 键 5 s 清除 peer（原工程同款操作），或清空 dongle 存储区。

## 与 ZMK dongle 版的行为差异

- ZMK receiver 完全禁止对主机 BLE 广播；本版的接收器已改为同款行为（rmk feature
  `ble_central_only`，现驻 lemon fork，提交 00988fd7）：接收器的无线电角色只剩两条 split 链路，
  对主机唯一出口是 USB。代价是主机侧"电量经 BLE 上报"与接收器上的 profile 槽切换失效
  （电量看半键盘屏，profile 键只影响半板历史遗留槽位，无实际作用）。
  如需恢复"接收器也能无线连主机"，去掉该 feature 并重编即可。
- ZMK receiver 禁用休眠；本版 dongle 沿用 `split_central_sleep_timeout_seconds = 300`
  的慢心跳策略（与 trouble 栈验证过的一致）。
- 接收器无电池：`BleBatteryConfig` 关闭；半键盘电量显示在各自的屏幕上（电量经 BLE
  GATT 上报主机的那条路随 `ble_central_only` 一并停用，USB 主机侧不再有电池服务消费者）。

## 来源

- 代码血统：`C:\Users\lemon\keypoint-dongle`（2026-09-09 整体并入本工程后删除——该工程
  唯一实错是接收器 0x10000 链接基址，功能代码本身完好）
- 依赖：Lemon 的 rmk fork `https://github.com/Iemooon/rmk`，本地 clone
  `C:\Users\lemon\Documents\GitHub\rmk`，path 依赖。fork 上携带本工程必需的
  `ble_central_only` feature（接收器禁主机广播）；上游同步节奏=把 upstream 拉进
  fork、保住补丁提交、这边重编
- dongle 结构参考：rmk 官方 `examples/use_rust/nrf52840_ble_split_dongle`
- 运行逻辑参考：`C:\Users\lemon\Documents\GitHub\keypoint-zmk-dongle`
