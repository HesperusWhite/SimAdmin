# 来电通知到 Telegram

来电提醒由 Rust 辅助组件 simadmin-call-notify 提供。当前 SimAdmin 主程序没有启用电话监听，因此单独增加通知规则不会产生来电推送。辅助组件复用 simadmin-notify 签名发送库，将来电交给本地桥接服务，再由桥接服务推送至 Telegram。

## 功能与设备范围

- 仅匹配型号 RG200U-CN 且网络端口为 usb0 的唯一模块；匹配不唯一时停止采集并重试。
- 物理串口继续由 ModemManager 独占。采集只调用 D-Bus 读取接口和固定的 AT+CLCC 查询。
- 忽略外呼和非语音呼叫；在来电响铃或呼叫等待时推送号码、北京时间及状态。
- 同一通电话持续响铃、转为通话中或服务重启时不会再次提醒。呼叫索引结束后复用、号码改变或 ModemManager 更换实例时按新来电处理。
- 号码暂未到达时最多等待 2 秒；仍不可知则显示“隐藏或未知号码”。
- 来电通知先以 0600 权限原子落盘，再用稳定 ID、HMAC 签名送入桥接 RX。只有收到 HTTP 202 且 accepted=true 才删除本地待发记录。
- 通知发送独立于来电采集；桥接暂时不可用时每 5 秒重试，桥接接受后由其持久队列负责 Telegram 发送。
- 接收 ModemManager 通话信号，并以 1.5 秒周期查询兜底；信号未上报且短于查询间隔的来电可能无法捕获。运营商已转走的来电也不会到达板子。

## 配置与启动

配置示例位于 crates/simadmin-call-notify/config.example.json，默认关闭。程序不持有 Telegram Token，仅读取现有桥接的 RX 签名密钥。

默认安装路径：

- 程序：/usr/bin/simadmin-call-notify
- 配置：/etc/simadmin-call-notify/config.json
- 状态：/opt/simadmin/data/call-notify
- procd：/etc/init.d/simadmin-call-notify

运行 check-config --config PATH 仅离线验证；probe --config PATH 仅读取当前模块和通话列表；enqueue-test --config PATH 向停止状态的服务队列写入一条明确标记的测试通知；run --config PATH 才开始监听及发送。

编译与检查：

~~~sh
cargo test --locked --offline -p simadmin-call-notify
cargo clippy --locked --offline -p simadmin-call-notify --all-targets -- -D warnings
cargo build --release --target aarch64-unknown-linux-musl --locked --offline -p simadmin-call-notify
~~~

测试覆盖内外呼过滤、重复响铃、呼叫索引复用、号码延迟、隐藏号码、持久去重、队列满、状态损坏及签名发送回执。真实来电验收使用另一部手机拨入此 SIM，响铃约 10 秒后挂断，检查 Bot 收到一条号码与时间正确的通知。

停止及回退只需停用新增的辅助服务：

~~~sh
/etc/init.d/simadmin-call-notify stop
/etc/init.d/simadmin-call-notify disable
~~~
