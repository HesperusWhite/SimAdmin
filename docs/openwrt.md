# Kwrt / OpenWrt 适配与离线构建

本适配为 aarch64 musl 平台的移远 RG200U-CN 模块（cdc_ncm / usb0）提供 procd/netifd 集成。仓库包含源码、测试与离线打包脚本；D-Bus / ModemManager 运行依赖需单独准备。

## 运行结构

保留 SimAdmin 的 ModemManager / system D-Bus 通信、短信接收、SQLite 去重存储和邮件通知队列。ModemManager 是唯一 AT 串口管理者；SimAdmin 通过其 D-Bus Command 方法控制模块，不另开串口抢占设备。ModemManager 需要以 `--debug` 启动以开放 Command 方法，配套 helper 在 D-Bus 就绪后调用 `SetLogging INFO` 降低日常日志级别；每次 procd 拉起 MM 都会重新执行此步骤；日志配置失败则结束本次 MM 子进程，避免持续输出 debug 日志。

Kwrt 使用 procd 监管两个独立服务：`simadmin-modemmanager` 和 `simadmin`。仓库提供的是源文件，两个 UCI 开关默认均为 `0`，没有创建开机启用链接。OpenWrt 下跳过 systemd debug override 和 systemd 自愈资产写入。

OpenWrt 的 ModemManager 1.24.0 包以 `-Dudev=false` 构建，需要显式报告端口。自有 wrapper 使用 `--no-auto-scan --test-no-qrtr --filter-policy=ALLOWLIST-ONLY`，仅允许 `usb0` 与 `ttyUSB2`。每次启动和每次报告前，从 sysfs 检查二者同属 `2c7c:0900`，接口号分别是 `00`、`04`；启动前只读检查 AT 串口 fd 占用，已有 sms_tool 或其他串口程序占用则拒绝竞争。`78-mm-simadmin-rg200u.rules` 只给这两个端口授权，不能使用会被 generic parser 忽略的 `99-mm-` 文件名。

启动顺序是：校验硬件与独占条件、启动 MM、确认 D-Bus owner 的 PID 就是该 MM 子进程、降低日志级别、报告两个端口，再等待唯一匹配的 RG200U 对象。等待总时限 60 秒，每轮间隔 1 秒；对象的物理路径、model、primary-port 与 ports 必须全部匹配，才对明确对象执行一次 Enable，并确认 state 已到 enabled 或以上。此步骤使 Messaging 可用，不调用 SimpleConnect，不建立数据 bearer，不读取短信内容，也不要求运营商 registered 状态。

自有服务不启动发行版 `ModemManager-monitor`，不重放全量 `events.cache`，不调用 netifd available/up 或全局 network reload。存在非空的原生 MM pidfile 时拒绝启动，避免 stock hotplug 向受限 MM 注入事件。离线包提供 `Exec=/bin/false` 的 D-Bus 激活文件，确保 MM 只能经显式 supervisor 启动；升级 MM 软件包后应重新审查该文件。当前只保证 wrapper 每次 procd respawn 时重新校验并报告两个端口，完整 USB 拔插恢复尚未实现。

蜂窝数据复用 netifd 的 `wwan_5g_0`（设备 `usb0`），APN 默认为 `3gnet`。只使用数据 PDP CID 1，不更改 IMS CID 11，不自动更改模块 USB/NAT 模式。数据功能只操作蜂窝模块及其关联网络接口；不得重启系统 network 服务、修改有线网卡、调整有线默认路由或全局防火墙。激活前只读检查蜂窝 IPv4 的 metric 至少为 600、peerdns/delegate 为 0，以及关联 IPv6 的 defaultroute/peerdns/delegate 均为 0；缺少这些配置会拒绝拨号，保证有线默认路由与系统 DNS 不被接管。

## 运行依赖与预检

运行依赖包括与该 Kwrt 固件/架构匹配的 D-Bus、ModemManager 及其 Quectel/通用插件、`dbus-send`、netifd、`ubus`、UCI、`jsonfilter`、`ip` 和 `pidof`。包名与拆包方式以当前固件软件源为准；运行依赖验证与 SimAdmin 应用部署分别管理。

运行时必须具备 `/var/run/dbus/system_bus_socket`、ubus 服务接口和 `org.freedesktop.ModemManager1` D-Bus owner。MM 还需要随包提供的 D-Bus policy 和设备识别规则。不能只复制 `/usr/sbin/ModemManager` 后假定模块可被发现。USB AT 端口应由 MM 发现为 RG200U 的 `/dev/ttyUSB2`，网卡为 `usb0`。

`simadmin modem-probe` 是一次性实机只读验证命令，读取绑定模块的网络、短信存储能力和四类呼转状态，不启动 Web 服务、不建立数据库、不删除短信或修改呼转。输出不含短信正文及 SIM 唯一标识。

`simadmin doctor` 是只读预检入口。它用于查看平台、依赖与接口状态，不执行拨号、安装、启停服务或修改系统配置。服务模板也会检查依赖、D-Bus owner 及旧拨号器竞争，失败时仅报错。

必须在获得后续部署授权后处理服务竞争：审查并停止/禁用旧的 `modem` 拨号服务，以及发行版自带的 `modemmanager` / `ModemManager` 服务，再使用此适配提供的 MM supervisor；不要重启全局 network 服务。若保留一个已满足 `--debug` / Command 要求的现有 MM 服务，可配置 `modem_service` 为 `modemmanager` 或 `ModemManager`，禁止同时启动第二个 MM。预检不会替用户自动停止其他服务。

## 编译与打包

在仓库根目录编译（使用目标平台对应的 GCC / Rust musl 工具链）：

```sh
cd backend
cargo build --locked --release --target aarch64-unknown-linux-musl -p simadmin
cd ../frontend
pnpm install --frozen-lockfile
pnpm run build
pnpm run type-check
cd ..
```

以下命令只生成新的审查目录，不执行其中的程序或 init 脚本：

```sh
scripts/openwrt/stage.sh
# 可指定已存在父目录下的全新输出目录：
scripts/openwrt/stage.sh "$PWD/build/openwrt-review"
```

输出默认位于 `build/openwrt-stage`，包含 `/opt/simadmin` 应用与前端、`/etc/init.d` 服务源文件、`/etc/config/simadmin` 默认配置、受限 MM wrapper/helper、两端口授权规则、禁止默认自动激活的 D-Bus 文件和 `SHA256SUMS`。已有输出目录会被拒绝，避免覆盖上一次审查产物。没有 `postinst`、安装钩子、服务 enable/start 或系统 network reload 操作。

以下为部署前需审查的**蜂窝专属 UCI 配置示例**。应用配置时保留现有 eth/LAN/WAN 设置：

```uci
config interface 'wwan_5g_0'
    option proto 'dhcp'
    option device 'usb0'
    option metric '600'
    option peerdns '0'
    option delegate '0'

config interface 'wwan6_5g_0'
    option proto 'dhcpv6'
    option device '@wwan_5g_0'
    option defaultroute '0'
    option peerdns '0'
    option delegate '0'
    option reqprefix 'no'
    option extendprefix '0'
```

纯脚本检查：

```sh
scripts/openwrt/test-stage.sh
```

该命令同时运行服务竞争与失败处理的行为测试，使用 PATH mocks 代替 ubus、D-Bus 和日志命令；覆盖物理路径/串口缺失、事件范围、Enable 与 enabled 状态、初始化超时、owner 丢失及失败清理。测试依赖已用于前端构建的 Node.js，不连接真实服务或网卡。产品 wrapper/helper 仅依赖 BusyBox sh、mmcli、dbus-send、jsonfilter 等系统工具，运行时不需要 Node.js。

Rust 功能测试使用 `cargo test --locked --workspace --target aarch64-unknown-linux-musl`；前端的 `pnpm run build`、`pnpm run type-check` 需要在 frontend 目录分别执行。`--help`、`--version` 和 `doctor` 可用于二进制冒烟；不要把 `serve` 当作无副作用的编译验证命令。

## 配置与持久化

`/etc/config/simadmin` 的 `main` 节提供：

| 配置 | 默认值 | 用途 |
| --- | --- | --- |
| enabled | 0 | 默认不启动应用 |
| binary | /opt/simadmin/simadmin | 应用路径 |
| data_dir | /opt/simadmin/data | SQLite、配置与认证数据持久化目录 |
| host / port | 127.0.0.1 / 3000 | 默认仅本机监听；对外访问地址后续按需配置 |
| modem_service | simadmin-modemmanager | 允许 simadmin-modemmanager、modemmanager、ModemManager；拒绝映射到 network 等服务 |
| network_interface | wwan_5g_0 | 只允许指定的蜂窝 netifd 接口 |
| ipv6_network_interface | wwan6_5g_0 | 关联的蜂窝 IPv6 接口，须验证其 device=@主 IPv4 接口 |
| network_device | usb0 | 限定为 usb0，其他设备会被拒绝 |
| apn | 3gnet | 应用 APN 配置为空时的默认值 |

`modemmanager` 节单独配置 MM binary 和默认关闭的 enabled。procd 导出 `SIMADMIN_PLATFORM=openwrt`、`SIMADMIN_DATA_DIR`、`SIMADMIN_MODEM_SERVICE`、`SIMADMIN_NETWORK`、`SIMADMIN_IPV6_NETWORK`、`SIMADMIN_INTERFACE`、`SIMADMIN_APN` 与 system D-Bus 地址。前端默认从二进制同目录的 `www` 提供。数据目录应落在持久化磁盘并仅允许 root 访问，不放在 `/tmp` 或 `/var/run`。`/opt` 的持久化挂载情况须在实际部署前确认。

短信转邮件沿用应用设置中的 SMTP、收件人、规则和重试队列；自动化投递测试使用本地模拟邮件服务器。呼叫转移通过运营商 `AT+CCFC` 服务实现，目标号码与转移类型需在实际使用时明确配置，不自动注册或修改真实 SIM 的呼叫转移。

## 更新策略

OpenWrt 下关闭上游 OTA 查询、下载、上传和应用入口，定时更新通知也跳过。原版 `3899/SimAdmin` OTA 包不包含本地 procd/netifd 适配，不能覆盖此分支。后续更新应重新构建、审查离线包，并在获得部署授权后安装；如果未来需要在线更新，应先建立独立的适配版签名渠道和安装流程。systemd 平台保持原有 OTA 行为。

## 呼叫转移接口与功能范围

呼转沿用已受登录保护的 `GET /api/call/forwarding` 和 `POST /api/call/forwarding`。POST 接收 `forward_type`、`number`、可选 `enabled` 与 `timeout`：

| forward_type | 含义 |
| --- | --- |
| unconditional | 无条件转移 |
| busy | 忙线转移 |
| no_reply | 无应答转移，timeout 默认 20 秒，范围 0–30 秒 |
| not_reachable | 不可达转移 |

号码仅允许数字及开头的 `+`，建议填写完整国际号码。非空号码默认注册并启用；空号码或 `enabled=false` 关闭该类语音转移，保留运营商登记号码。修改只针对语音 class 1，随后查询四种状态，核验启用状态、目标号码和无应答超时；运营商拒绝、响应异常或结果不一致都会返回错误，避免把未生效的设置显示为成功。运营商是否开通此业务需要实际使用前确认。

本次提供呼转 API，没有新增前端呼转设置页，也没有实现网络语音中继或录音。短信转邮件使用现有通知设置页面。Kwrt 的 Wi-Fi 由 LuCI 管理；通用基带重启、SIM 断电和自动 QMI 恢复入口在该适配中关闭，蜂窝恢复使用 CID 1 重新连接。

## 模块兼容性说明

当固件的 `AT+GCAP` 仅声明 `+GSM` 时，ModemManager 的注册摘要可能与实际 5G 数据状态不一致。受限 supervisor 的就绪条件使用设备身份与 enabled 状态；应用通过 AT 注册信息交叉校验网络状态，蜂窝拨号由适配层的 5G 注册查询和 netifd 配置检查决定。
