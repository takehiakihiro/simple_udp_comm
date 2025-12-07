use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::time::{self, Duration};

const PORT: u16 = 4000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MsgKind {
    Data,
    Fin,
}

fn default_kind() -> MsgKind {
    MsgKind::Data
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Message {
    no: u32,
    retry: u32,
    from: String, // "client" or "server"
    #[serde(default = "default_kind")]
    kind: MsgKind, // "data" or "fin"
}

/// 受信した no を記録して、「どこからどこまで受信済みか」を表示するための構造体
#[derive(Default)]
struct RecvLog {
    label: String,
    received: BTreeSet<u32>,
}

impl RecvLog {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            received: BTreeSet::new(),
        }
    }

    fn record(&mut self, no: u32) {
        self.received.insert(no);

        let summary = self.build_ranges_summary();
        let min = self.received.first().copied().unwrap_or(0);
        let max = self.received.last().copied().unwrap_or(0);
        let total = self.received.len();

        println!(
            "[{}] 受信ログ: min={}, max={}, total={}, ranges=[{}]",
            self.label, min, max, total, summary
        );
    }

    /// 受信済みの no を、連続区間ごとに "1-5, 7-10, 12" のような文字列にする
    fn build_ranges_summary(&self) -> String {
        let mut ranges = Vec::new();
        let mut start: Option<u32> = None;
        let mut prev: Option<u32> = None;

        for &n in &self.received {
            match (start, prev) {
                (None, _) => {
                    start = Some(n);
                    prev = Some(n);
                }
                (Some(s), Some(p)) => {
                    if n == p + 1 {
                        // 連続
                        prev = Some(n);
                    } else {
                        // 途切れたのでひと区間確定
                        if s == p {
                            ranges.push(format!("{}", s));
                        } else {
                            ranges.push(format!("{}-{}", s, p));
                        }
                        start = Some(n);
                        prev = Some(n);
                    }
                }
                _ => {}
            }
        }

        if let (Some(s), Some(p)) = (start, prev) {
            if s == p {
                ranges.push(format!("{}", s));
            } else {
                ranges.push(format!("{}-{}", s, p));
            }
        }

        ranges.join(", ")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("使い方:");
        eprintln!("  サーバ:  {} -s", args[0]);
        eprintln!("  クライアント: {} -c <server_ip>", args[0]);
        std::process::exit(1);
    }

    match args[1].as_str() {
        "-s" => {
            run_server().await?;
        }
        "-c" => {
            if args.len() < 3 {
                eprintln!("クライアントモードにはサーバIPが必要です。");
                eprintln!("例: {} -c 127.0.0.1", args[0]);
                std::process::exit(1);
            }
            let ip = &args[2];
            let addr: SocketAddr = format!("{}:{}", ip, PORT).parse()?;
            run_client(addr).await?;
        }
        _ => {
            eprintln!("不明なオプション: {}", args[1]);
            eprintln!("  -s : サーバモード");
            eprintln!("  -c : クライアントモード");
            std::process::exit(1);
        }
    }

    Ok(())
}

/// クライアント処理
async fn run_client(server_addr: SocketAddr) -> Result<(), Box<dyn Error>> {
    println!("クライアント起動: サーバ = {}", server_addr);

    // ローカル側は適当なポートでバインド
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect(server_addr).await?;

    let mut no: u32 = 1;
    let mut retry: u32 = 0;

    // サーバからの応答番号の受信ログ
    let mut recv_log = RecvLog::new("CLIENT");

    while no <= 100 {
        let msg = Message {
            no,
            retry,
            from: "client".to_string(),
            kind: MsgKind::Data,
        };
        let data = serde_json::to_vec(&msg)?;
        println!("[CLIENT] 送信: {:?}", msg);
        socket.send(&data).await?;

        let mut buf = [0u8; 1024];

        // サーバからの応答を100ms待つ
        match time::timeout(Duration::from_millis(100), socket.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                match serde_json::from_str::<Message>(&text) {
                    Ok(reply) => {
                        println!("[CLIENT] 受信: {:?}", reply);

                        if let MsgKind::Data = reply.kind {
                            // データメッセージだけログに記録
                            recv_log.record(reply.no);
                        }

                        if matches!(reply.kind, MsgKind::Data)
                            && reply.from == "server"
                            && reply.no == no
                        {
                            // この no に対する応答が来たので次の番号へ
                            if no == 100 {
                                println!(
                                    "[CLIENT] no=100 の応答を受信。FIN を送信して終了します。"
                                );

                                // FIN を送信（no=0 は特別な意味として使用）
                                let fin = Message {
                                    no: 0,
                                    retry: 0,
                                    from: "client".to_string(),
                                    kind: MsgKind::Fin,
                                };
                                let fin_data = serde_json::to_vec(&fin)?;
                                println!("[CLIENT] FIN 送信: {:?}", fin);
                                socket.send(&fin_data).await?;

                                break;
                            }
                            no += 1;
                            retry = 0;
                        } else {
                            // 想定と違うメッセージなら無視してリトライカウントを増やす
                            eprintln!(
                                "[CLIENT] 想定外のメッセージ (kind={:?}, from={}, no={}), リトライします",
                                reply.kind, reply.from, reply.no
                            );
                            retry += 1;
                        }
                    }
                    Err(e) => {
                        eprintln!("[CLIENT] JSON パースエラー: {} / 生データ: {}", e, text);
                        retry += 1;
                    }
                }
            }
            Ok(Err(e)) => {
                eprintln!("[CLIENT] recv エラー: {}", e);
                retry += 1;
            }
            Err(_) => {
                // タイムアウト
                retry += 1;
                println!(
                    "[CLIENT] タイムアウト: no={}, retry={} で再送します",
                    no, retry
                );
            }
        }
    }

    println!("[CLIENT] 終了");
    Ok(())
}

/// サーバ処理
async fn run_server() -> Result<(), Box<dyn Error>> {
    let bind_addr = format!("0.0.0.0:{}", PORT);
    let socket = UdpSocket::bind(&bind_addr).await?;
    println!("[SERVER] 起動: {}", bind_addr);

    let mut last_msg: Option<Message> = None;
    let mut last_addr: Option<SocketAddr> = None;

    // クライアントから受信した no のログ
    let mut recv_log = RecvLog::new("SERVER-RECV");

    loop {
        let mut buf = [0u8; 1024];

        // クライアントからのデータを 100ms 待つ
        match time::timeout(Duration::from_millis(100), socket.recv_from(&mut buf)).await {
            // 受信できた
            Ok(Ok((n, addr))) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                match serde_json::from_str::<Message>(&text) {
                    Ok(msg) => {
                        println!("[SERVER] 受信 from {}: {:?}", addr, msg);

                        match msg.kind {
                            MsgKind::Data => {
                                // 受信ログを更新
                                recv_log.record(msg.no);

                                // クライアントから来た no をそのまま返す
                                let reply = Message {
                                    no: msg.no,
                                    retry: 0, // 新規応答なので retry=0
                                    from: "server".to_string(),
                                    kind: MsgKind::Data,
                                };
                                let data = serde_json::to_vec(&reply)?;
                                socket.send_to(&data, addr).await?;
                                println!("[SERVER] 送信 to {}: {:?}", addr, reply);

                                // 再送用に記録
                                last_msg = Some(reply);
                                last_addr = Some(addr);
                            }
                            MsgKind::Fin => {
                                println!("[SERVER] FIN 受信 from {}: {:?}", addr, msg);
                                println!("[SERVER] セッションを終了します。");
                                // ここでプロセス終了（ループを抜ける）
                                return Ok(());
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[SERVER] JSON パースエラー: {} / 生データ: {}", e, text);
                    }
                }
            }
            // recv_from 自体のエラー
            Ok(Err(e)) => {
                eprintln!("[SERVER] recv_from エラー: {}", e);
            }
            // タイムアウト: 直前のメッセージを retry+1 して再送
            Err(_) => {
                if let (Some(mut msg), Some(addr)) = (last_msg.clone(), last_addr) {
                    // 直前に送信したメッセージが Data の場合だけ再送（FIN は再送しない）
                    if matches!(msg.kind, MsgKind::Data) {
                        msg.retry += 1;
                        let data = serde_json::to_vec(&msg)?;
                        println!("[SERVER] タイムアウト、再送 to {}: {:?}", addr, msg);
                        socket.send_to(&data, addr).await?;
                        last_msg = Some(msg);
                    }
                } else {
                    // まだ何も送ったことがない場合は何もしない
                }
            }
        }
    }
}
