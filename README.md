# smallchat-rs

[antirez/smallchat](https://github.com/antirez/smallchat) 的 Rust 移植，包含 TCP 聊天服务端和终端客户端。参考上游提交 `8fc6d387ceb9e1dab83182e7b89885e0f1b43f5a`。

保留原项目的单进程、单线程事件循环，通过 `poll` 监听连接和输入事件。网络连接、内存和缓冲区使用 Rust 标准库管理；唯一依赖 `libc` 用于 Unix 的 `poll`、终端设置及标准输入读取，相关 `unsafe` 调用均标注安全前提。支持 macOS、Linux，需要 Rust 1.74 或更新版本。

## 运行

在本目录中编译并启动服务端：

```sh
cargo build --release
cargo run --release --bin smallchat-server
```

默认监听 `0.0.0.0:7711`，也可以指定地址：

```sh
cargo run --release --bin smallchat-server -- 127.0.0.1:7711
```

另开两个或更多终端，分别连接：

```sh
cargo run --release --bin smallchat-client -- 127.0.0.1 7711
```

输入 `/nick alice` 设置昵称，之后输入文字并按回车发送。其他连接会收到 `alice> 消息`；发送方由终端客户端显示 `you> 消息`。默认昵称为 `user:<socket fd>`。支持退格删除 UTF-8 字符，收到新消息时重绘当前输入；Ctrl-C 或 Ctrl-D 退出并恢复终端设置。不支持方向键编辑、历史记录或多行输入。

也可直接使用 netcat，或通过管道调用 Rust 客户端：

```sh
nc 127.0.0.1 7711
printf '/nick bot\nhello\n' | target/release/smallchat-client 127.0.0.1 7711
```

管道输入结束后会发送未换行的最后一条消息，关闭 TCP 写方向，读取剩余服务端数据直到连接关闭。两个程序均支持 `--help`。

## 与 C 版本的对应关系

| C 源码 | Rust 实现 |
| --- | --- |
| `smallchat-server.c` | `src/bin/server.rs`：连接管理、命令解析、消息广播 |
| `smallchat-client.c` | `src/bin/client.rs`：终端模式、输入编辑、收发事件循环 |
| `chatlib.c` / `chatlib.h` | `std::net`、Rust 自动资源管理及 `src/lib.rs` 的就绪事件封装 |

保留欢迎消息、`/nick <nick>`、未知命令错误、默认端口及排除发送方的广播行为。改进以下边界行为：

- 按换行符分隔消息，正确处理 TCP 拆包、粘包，兼容 LF 和 CRLF。未换行的数据会缓冲到换行或连接 EOF，因此交互使用 netcat 时需要按回车。
- 使用收发缓冲区处理非阻塞 I/O 和部分写入，不再将消息截断到 255 字节。
- 最多同时连接 1000 个客户端；每行最多 4096 字节（不含 LF，包含可选 CR），超长输入会断开连接。
- 昵称必须为有效 UTF-8、非空白、无控制字符，长度最多 128 字节。消息内容按原始字节转发。
- 每个连接最多积压 64 KiB 输出；超限会断开该慢客户端，其他连接继续工作。
- 客户端采用 RAII 恢复终端；正常退出、Ctrl-C、Ctrl-D 和 I/O 错误均会恢复设置。外部强制终止进程不经过此恢复逻辑。

## 验证

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

集成测试在本机随机端口启动真实服务端，验证多人广播、发送方不回显、默认昵称与改名、未知命令、TCP 分片和合并、中文、长消息、超长连接隔离、断开重连，以及 Rust 客户端管道输入与 EOF 行为。

项目沿用上游 BSD 3-Clause 许可证，版权声明与许可条款见 [LICENSE](LICENSE)。
