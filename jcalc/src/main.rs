use std::collections::VecDeque;
use std::io::IsTerminal;
use std::io::Write;
use std::io::stdout;
use std::sync::Arc;

use clap::Parser;
use ore_jdwp::defs::ClassTypeInvokeMethodReceive;
use ore_jdwp::defs::ClassTypeInvokeMethodSend;
use ore_jdwp::defs::ClassTypeInvokeMethodSendArguments;
use ore_jdwp::defs::ClassTypeNewInstanceReceive;
use ore_jdwp::defs::ClassTypeNewInstanceSend;
use ore_jdwp::defs::ClassTypeNewInstanceSendArguments;
use ore_jdwp::defs::EventCompositeReceive;
use ore_jdwp::defs::EventCompositeReceiveEventsEventKind;
use ore_jdwp::defs::EventRequestSetSend;
use ore_jdwp::defs::EventRequestSetSendModifiers;
use ore_jdwp::defs::EventRequestSetSendModifiersModKind;
use ore_jdwp::defs::EventRequestSetSendModifiersModKind1;
use ore_jdwp::defs::EventRequestSetSendModifiersModKind3;
use ore_jdwp::defs::ObjectReferenceDisableCollectionSend;
use ore_jdwp::defs::ObjectReferenceEnableCollectionSend;
use ore_jdwp::defs::ObjectReferenceInvokeMethodReceive;
use ore_jdwp::defs::ObjectReferenceInvokeMethodSend;
use ore_jdwp::defs::ObjectReferenceInvokeMethodSendArguments;
use ore_jdwp::defs::ReferenceTypeMethodsReceive;
use ore_jdwp::defs::ReferenceTypeMethodsReceiveDeclared;
use ore_jdwp::defs::ReferenceTypeMethodsSend;
use ore_jdwp::defs::StringReferenceValueReceive;
use ore_jdwp::defs::StringReferenceValueSend;
use ore_jdwp::defs::VirtualMachineClassesBySignatureReceive;
use ore_jdwp::defs::VirtualMachineClassesBySignatureSend;
use ore_jdwp::defs::VirtualMachineCreateStringReceive;
use ore_jdwp::defs::VirtualMachineCreateStringSend;
use ore_jdwp::packets::JDWPEventKindConstants;
use ore_jdwp::packets::JDWPIDLengthEqMethod;
use ore_jdwp::packets::JDWPIDLengthEqObject;
use ore_jdwp::packets::JDWPIDLengthEqReferenceType;
use ore_jdwp::packets::JDWPTaggedObjectID;
use ore_jdwp::packets::JDWPValue;
use ore_jdwp::packets::PacketFromDebuggeeError;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use ore_jdwp::packets::{JDWPContext, JDWPPacketDataFromDebuggee, JDWPPacketDataFromDebugger};
use ore_jdwp::packets::{parse_packet_from_debuggee, send_packet};

mod parse;

#[derive(Parser, Debug)]
#[command(name = "tcp_client")]
struct Args {
  #[arg(
    short = 'H',
    long,
    default_value = "127.0.0.1",
    help = "Target jvm host to connect to"
  )]
  host: String,

  #[arg(
    short,
    long,
    default_value = "5005",
    help = "Target jvm port to connect to"
  )]
  port: u16,

  #[arg(short, long, help = "Enable verbose output")]
  verbose: bool,

  // "-3*2" のように単項マイナスで始まる式をオプションと誤認させないため
  #[arg(
    short,
    long,
    allow_hyphen_values = true,
    help = "If set, calc desinated expression and exit"
  )]
  expression: Option<String>,
}

// 受信タスクから送信タスクへ渡すもの。エラー応答も send_and_receive の呼び出し元へ届けるため Result にする。
// 送信側で VM_DEAD などのエラーコードを判別できるよう、文字列にせずそのまま渡す
type Received = Result<JDWPPacketDataFromDebuggee, PacketFromDebuggeeError>;

const JDWP_ERROR_VM_DEAD: u16 = 112;
const JDWP_SUSPEND_POLICY_NONE: i8 = 0;
const JDWP_SUSPEND_POLICY_ALL: i8 = 2;

// 受信パケットのデコードに、どのコマンドへの応答かを引くため送信済みペイロードを共有する
type Payloads = Arc<Mutex<Vec<JDWPPacketDataFromDebugger>>>;

const HANDSHAKE: &[u8; 14] = b"JDWP-Handshake";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let args = Args::parse();

  let mut stream = TcpStream::connect((args.host.as_str(), args.port)).await?;
  if args.verbose {
    eprintln!("Connected to {}:{}", args.host, args.port);
  }

  let payloads: Payloads = Arc::new(Mutex::new(Vec::new()));

  // --- Handshake ---
  stream.write_all(HANDSHAKE).await?;
  stream.flush().await?;
  if args.verbose {
    eprintln!("Sent handshake: {:?}", String::from_utf8_lossy(HANDSHAKE));
  }

  // ハンドシェイクの応答は JDWP パケットの形式ではないため、受信タスクを起動する前に直接読む
  let mut buf = [0u8; HANDSHAKE.len()];
  stream.read_exact(&mut buf).await?;
  if &buf != HANDSHAKE {
    return Err(Box::from("Invalid handshake response"));
  }
  if args.verbose {
    eprintln!("Handshake successful!");
  }

  // --- ここから非同期で送受信を分離 ---
  let (reader, writer) = stream.into_split();
  // 受信タスクから送信タスクへのチャネル
  let (channel_tx, channel_rx) = mpsc::channel::<Received>(8192);

  // 受信タスク
  let recv_task = tokio::spawn(handle_receive(reader, Arc::clone(&payloads), channel_tx));

  // 送信タスク
  let send_task = tokio::spawn(handle_send(writer, payloads, channel_rx, args));

  let send_result = send_task.await?;
  // JVM 側が接続を切らない限り受信タスクは終わらないため、計算が終わった時点で止める
  recv_task.abort();
  if let Err(e) = send_result {
    eprintln!("{}", e);
    std::process::exit(1);
  }
  Ok(())
}

async fn handle_receive(
  mut reader: tokio::net::tcp::OwnedReadHalf,
  payloads: Payloads,
  channel_tx: mpsc::Sender<Received>,
) {
  // IDSizes の応答の直後に届くパケットからデコードに使うため、送信側を待たずにここで設定する
  let mut context = JDWPContext { id_sizes: None };
  // ID サイズが決まる前に届いてデコードできなかったイベント。
  // ハンドシェイク直後に届く VMStart を取りこぼさないよう、決まった後にデコードし直す
  let mut undecoded_events: Vec<Vec<u8>> = Vec::new();

  while let Ok(length) = reader.read_u32().await {
    // length は自身の 4 バイトを含む
    let Some(body_length) = (length as usize).checked_sub(4) else {
      break;
    };
    let mut buf = vec![0u8; body_length];
    if reader.read_exact(&mut buf).await.is_err() {
      break;
    }

    let packet_and_id = parse_packet_from_debuggee(&payloads.lock().await, &buf, &context);

    let received = match packet_and_id {
      Ok((packet, _)) => {
        if let JDWPPacketDataFromDebuggee::VirtualMachineIDSizes(id_sizes) = &packet {
          context.set_from_id_sizes_response(id_sizes);
        }
        Ok(packet)
      }
      Err(_) if context.id_sizes.is_none() && !is_reply(&buf) => {
        undecoded_events.push(buf);
        continue;
      }
      // 応答待ちの send_and_receive を止めないよう、応答の失敗だけを伝える
      Err(e) if is_reply(&buf) => Err(e),
      // イベントは購読しているもの以外読めなくても困らないので捨てる
      Err(_) => continue,
    };

    if channel_tx.send(received).await.is_err() {
      break;
    }

    // 保留したイベントは実際より遅れて IDSizes の応答の後に流れるが、送信側はイベントを応答と区別して
    // 後から拾うので問題ない。次のパケットを読む前に流すため、イベント同士の順序は保たれる
    if context.id_sizes.is_some() {
      for buf in std::mem::take(&mut undecoded_events) {
        let packet_and_id = parse_packet_from_debuggee(&payloads.lock().await, &buf, &context);
        if let Ok((packet, _)) = packet_and_id
          && channel_tx.send(Ok(packet)).await.is_err()
        {
          return;
        }
      }
    }
  }
}

fn is_reply(buf_without_length: &[u8]) -> bool {
  // id (4 バイト) の次が flags
  buf_without_length
    .get(4)
    .is_some_and(|flags| flags & 0x80 != 0)
}

fn describe_receive_error(e: PacketFromDebuggeeError) -> String {
  match e {
    PacketFromDebuggeeError::ResponseWithErrorCode(code) => {
      format!("JDWP error {} ({})", code, jdwp_error_name(code))
    }
    e => format!("Failed to decode reply: {:?}", e),
  }
}

// 起こりうるものだけ名前を付け、他は番号のみで済ませる
fn jdwp_error_name(code: u16) -> &'static str {
  match code {
    10 => "INVALID_THREAD",
    13 => "THREAD_NOT_SUSPENDED",
    20 => "INVALID_OBJECT",
    21 => "INVALID_CLASS",
    23 => "INVALID_METHODID",
    34 => "INVALID_TYPE",
    JDWP_ERROR_VM_DEAD => "VM_DEAD",
    502 => "INVALID_EVENT_TYPE",
    503 => "ILLEGAL_ARGUMENT",
    506 => "NOT_IMPLEMENTED",
    _ => "unknown",
  }
}

async fn handle_send(
  writer: tokio::net::tcp::OwnedWriteHalf,
  payloads: Payloads,
  channel_rx: mpsc::Receiver<Received>,
  args: Args,
) -> Result<(), String> {
  let Args {
    expression,
    verbose,
    ..
  } = args;

  let log = Log { verbose };
  let mut h = SendHandler {
    writer,
    payloads,
    channel_rx,
    cmd_id: 0,
    pending_events: VecDeque::new(),
    throwable: None,
    connection_lost: false,
    log,
  };

  log.doing("Get id sizes");
  h.get_id_sizes().await?;
  log.done();

  // メインクラスの名前に依らず計算用スレッドを決めるため、必ず届く VMStart から main を実行する初期スレッドを得る
  log.doing("Wait for VM start");
  let initial_thread = h.wait_for_vm_start().await?;
  log.done();

  // VMStart で止まったスレッドでは InvokeMethod できない (INVALID_THREAD) ため、通常のイベントで止め直す。
  // 初期スレッドでは少なくともメインクラスが prepare されるので、このイベントは必ず届く
  log.doing("Set class prepare request");
  h.send_and_receive(&JDWPPacketDataFromDebugger::EventRequestSet(
    EventRequestSetSend {
      suspend_policy: JDWP_SUSPEND_POLICY_ALL,
      modifiers: vec![
        EventRequestSetSendModifiers {
          mod_kind: EventRequestSetSendModifiersModKind::_3(EventRequestSetSendModifiersModKind3 {
            thread: initial_thread,
          }),
        },
        // 計算中の InvokeMethod で起きるクラスの準備で VM が止まらないよう、一度きりにする
        EventRequestSetSendModifiers {
          mod_kind: EventRequestSetSendModifiersModKind::_1(EventRequestSetSendModifiersModKind1 {
            count: 1,
          }),
        },
      ],
      event_kind: JDWPEventKindConstants::CLASSPREPARE as i8,
    },
  ))
  .await?;
  log.done();

  log.doing("Resume VM");
  h.send_and_receive(&JDWPPacketDataFromDebugger::VirtualMachineResume(()))
    .await?;
  log.done();

  log.doing("Wait for class prepare");
  let current_thread = h.wait_for_class_prepare().await?;
  log.done();
  log.info(&format!("Current thread id: {}", current_thread));

  // 例外の内容を表示できるよう、以降の呼び出しより先に用意する
  log.doing("Find java.lang.Throwable");
  let clazz_throwable = h.find_class("Ljava/lang/Throwable;").await?;
  let throwable_methods = h.get_methods(&clazz_throwable).await?;
  h.throwable = Some(ThrowableMethods {
    to_string: find_method(&throwable_methods, "toString", "()Ljava/lang/String;")?,
    clazz: clazz_throwable,
  });
  log.done();

  log.doing("Find java.lang.Class");
  let clazz_of_class = h.find_class("Ljava/lang/Class;").await?;
  log.done();
  log.doing("Find Class.forName");
  let method_class_for_name = find_method(
    &h.get_methods(&clazz_of_class).await?,
    "forName",
    "(Ljava/lang/String;)Ljava/lang/Class;",
  )?;
  log.done();
  // ClassesBySignature はロード済みのクラスしか返さないため、forName で BigInteger をロードさせる
  log.doing("Load java.math.BigInteger");
  let string_big_integer = h.load_string("java.math.BigInteger").await?;
  h.invoke_class_method_return_object(
    &clazz_of_class,
    &method_class_for_name,
    &current_thread,
    &[JDWPValue::Object(string_big_integer)],
  )
  .await?;
  log.done();

  log.doing("Find java.math.BigInteger");
  let clazz_big_integer = h.find_class("Ljava/math/BigInteger;").await?;
  log.done();

  log.doing("Find BigInteger methods");
  let methods = h.get_methods(&clazz_big_integer).await?;
  let big_integer = BigIntegerMethods {
    new_from_string: find_method(&methods, "<init>", "(Ljava/lang/String;)V")?,
    add: find_method(&methods, "add", BIG_INTEGER_BINARY_OP_SIGNATURE)?,
    subtract: find_method(&methods, "subtract", BIG_INTEGER_BINARY_OP_SIGNATURE)?,
    multiply: find_method(&methods, "multiply", BIG_INTEGER_BINARY_OP_SIGNATURE)?,
    divide: find_method(&methods, "divide", BIG_INTEGER_BINARY_OP_SIGNATURE)?,
    negate: find_method(&methods, "negate", "()Ljava/math/BigInteger;")?,
    to_string: find_method(&methods, "toString", "()Ljava/lang/String;")?,
    clazz: clazz_big_integer,
  };
  log.done();

  let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());

  // -e 指定時と stdin が TTY でないときは、1 式だけ計算して終了する
  let one_shot = match expression {
    Some(expr) => Some(expr),
    None if !std::io::stdin().is_terminal() => {
      let mut expr = String::new();
      stdin
        .read_line(&mut expr)
        .await
        .map_err(|e| format!("Failed to read stdin: {}", e))?;
      Some(expr)
    }
    None => None,
  };

  if let Some(expr) = one_shot {
    let result = h
      .calc_expression(&expr, &big_integer, &current_thread)
      .await?;
    print!("{}", result);
  } else {
    let mut input = String::new();
    loop {
      print!("jcalc> ");
      stdout().flush().unwrap();
      input.clear();
      let read = stdin
        .read_line(&mut input)
        .await
        .map_err(|e| format!("Failed to read stdin: {}", e))?;
      if read == 0 {
        // EOF (Ctrl-D / Ctrl-Z) で終了する。プロンプトの後ろで改行しておく
        println!();
        break;
      }
      let line = input.trim();
      if line == "exit" {
        break;
      }
      if line.is_empty() {
        continue;
      }

      match h.calc_expression(line, &big_integer, &current_thread).await {
        Ok(result) => {
          println!("{}", result);
        }
        // 接続が切れていれば以降の計算もすべて失敗するため、対話を続けない
        Err(e) if h.connection_lost => return Err(e),
        Err(e) => {
          eprintln!("{}", e);
        }
      }
    }
  }
  Ok(())
}

// -v 指定時だけ進捗を stderr に出す
#[derive(Clone, Copy)]
struct Log {
  verbose: bool,
}

impl Log {
  // 後に done() で "..OK!" を続けるため改行しない
  fn doing(&self, what: &str) {
    if self.verbose {
      eprint!("* {}", what);
    }
  }

  fn doing_ln(&self, what: &str) {
    if self.verbose {
      eprintln!("* {}..", what);
    }
  }

  fn done(&self) {
    if self.verbose {
      eprintln!("..OK!");
    }
  }

  fn info(&self, info: &str) {
    if self.verbose {
      eprintln!("* {}", info);
    }
  }
}

// add / subtract / multiply / divide 共通のシグネチャ
const BIG_INTEGER_BINARY_OP_SIGNATURE: &str = "(Ljava/math/BigInteger;)Ljava/math/BigInteger;";

// 計算に使う、対象 JVM 上の BigInteger のクラスとメソッド
struct BigIntegerMethods {
  clazz: JDWPIDLengthEqReferenceType,
  new_from_string: JDWPIDLengthEqMethod,
  add: JDWPIDLengthEqMethod,
  subtract: JDWPIDLengthEqMethod,
  multiply: JDWPIDLengthEqMethod,
  divide: JDWPIDLengthEqMethod,
  negate: JDWPIDLengthEqMethod,
  to_string: JDWPIDLengthEqMethod,
}

// 例外の内容を文字列にするための、対象 JVM 上の Throwable のクラスとメソッド
#[derive(Clone)]
struct ThrowableMethods {
  clazz: JDWPIDLengthEqReferenceType,
  to_string: JDWPIDLengthEqMethod,
}

struct SendHandler {
  writer: tokio::net::tcp::OwnedWriteHalf,
  payloads: Payloads,
  channel_rx: mpsc::Receiver<Received>,
  cmd_id: i32,
  // 応答待ちの間に届いたイベント。wait_for_vm_start が応答より後に呼ばれても取りこぼさないよう保持する
  pending_events: VecDeque<EventCompositeReceive>,
  // 初期化の途中で解決するまでは None
  throwable: Option<ThrowableMethods>,
  // 以降の通信がすべて失敗する状態か。対話モードを打ち切る判断に使う
  connection_lost: bool,
  log: Log,
}

impl SendHandler {
  async fn send_and_receive(
    &mut self,
    payload: &JDWPPacketDataFromDebugger,
  ) -> Result<JDWPPacketDataFromDebuggee, String> {
    // payloads の添字と cmd_id を一致させるため、送信に失敗しても ID は進める
    let id = self.cmd_id;
    self.cmd_id += 1;
    self.payloads.lock().await.push(payload.clone());
    if let Err(e) = send_packet(&mut self.writer, id, payload).await {
      self.connection_lost = true;
      return Err(format!("Failed to send packet: {}", e));
    }

    loop {
      match self.recv().await? {
        JDWPPacketDataFromDebuggee::EventComposite(event_composite) => {
          self.pending_events.push_back(event_composite);
        }
        response_packet => return Ok(response_packet),
      }
    }
  }

  async fn next_event(&mut self) -> Result<EventCompositeReceive, String> {
    if let Some(event_composite) = self.pending_events.pop_front() {
      return Ok(event_composite);
    }
    loop {
      // 応答は送信したコマンドに対してしか来ないため、イベント以外は来ない
      if let JDWPPacketDataFromDebuggee::EventComposite(event_composite) = self.recv().await? {
        return Ok(event_composite);
      }
    }
  }

  // VM の終了とチャネルの切断は以降の通信がすべて失敗する状態なので、受信の入口でまとめて検出する
  async fn recv(&mut self) -> Result<JDWPPacketDataFromDebuggee, String> {
    match self.channel_rx.recv().await {
      Some(Ok(JDWPPacketDataFromDebuggee::EventComposite(event_composite)))
        if is_vm_death(&event_composite) =>
      {
        self.connection_lost = true;
        Err("VM DEATH".into())
      }
      Some(Ok(packet)) => Ok(packet),
      Some(Err(e)) => {
        if matches!(
          e,
          PacketFromDebuggeeError::ResponseWithErrorCode(JDWP_ERROR_VM_DEAD)
        ) {
          self.connection_lost = true;
        }
        Err(describe_receive_error(e))
      }
      None => {
        self.connection_lost = true;
        Err("Channel closed".into())
      }
    }
  }

  async fn wait_for_vm_start(&mut self) -> Result<JDWPIDLengthEqObject, String> {
    loop {
      let event_composite = self.next_event().await?;
      for event in event_composite.events {
        if let EventCompositeReceiveEventsEventKind::_VMSTART(vm_start) = event.event_kind {
          // suspend=n で起動された VM ではスレッドが止まっておらず、InvokeMethod に使えない
          if event_composite.suspend_policy == JDWP_SUSPEND_POLICY_NONE {
            return Err("VM is not suspended at start. Launch the JVM with suspend=y".into());
          }
          return Ok(vm_start.thread);
        }
      }
    }
  }

  async fn wait_for_class_prepare(&mut self) -> Result<JDWPIDLengthEqObject, String> {
    loop {
      let event_composite = self.next_event().await?;
      for event in event_composite.events {
        if let EventCompositeReceiveEventsEventKind::_CLASSPREPARE(class_prepare) = event.event_kind
        {
          return Ok(class_prepare.thread);
        }
      }
    }
  }

  // ID サイズの設定は受信タスクが応答を受けた時点で行うので、ここでは要求して応答を確かめるだけ
  async fn get_id_sizes(&mut self) -> Result<(), String> {
    let JDWPPacketDataFromDebuggee::VirtualMachineIDSizes(_) = self
      .send_and_receive(&JDWPPacketDataFromDebugger::VirtualMachineIDSizes(()))
      .await?
    else {
      return Err("Unexpected reply to IDSizes".into());
    };
    Ok(())
  }

  async fn load_string(&mut self, s: &str) -> Result<JDWPIDLengthEqObject, String> {
    let JDWPPacketDataFromDebuggee::VirtualMachineCreateString(VirtualMachineCreateStringReceive {
      string_object: str,
    }) = self
      .send_and_receive(&JDWPPacketDataFromDebugger::VirtualMachineCreateString(
        VirtualMachineCreateStringSend { utf: s.into() },
      ))
      .await?
    else {
      return Err("Unexpected reply to CreateString".into());
    };
    Ok(str)
  }

  async fn get_string_value(
    &mut self,
    string_object: &JDWPIDLengthEqObject,
  ) -> Result<String, String> {
    let JDWPPacketDataFromDebuggee::StringReferenceValue(StringReferenceValueReceive {
      string_value,
    }) = self
      .send_and_receive(&JDWPPacketDataFromDebugger::StringReferenceValue(
        StringReferenceValueSend {
          string_object: string_object.clone(),
        },
      ))
      .await?
    else {
      return Err("Unexpected reply to StringReference.Value".into());
    };
    Ok(string_value.data)
  }

  async fn find_class(&mut self, signature: &str) -> Result<JDWPIDLengthEqReferenceType, String> {
    let JDWPPacketDataFromDebuggee::VirtualMachineClassesBySignature(
      VirtualMachineClassesBySignatureReceive { classes },
    ) = self
      .send_and_receive(
        &JDWPPacketDataFromDebugger::VirtualMachineClassesBySignature(
          VirtualMachineClassesBySignatureSend {
            signature: signature.into(),
          },
        ),
      )
      .await?
    else {
      return Err("Unexpected reply to ClassesBySignature".into());
    };
    classes
      .into_iter()
      .next()
      .map(|class| class.type_id)
      .ok_or_else(|| format!("Class {} not found", signature))
  }

  async fn get_methods(
    &mut self,
    class_id: &JDWPIDLengthEqReferenceType,
  ) -> Result<Vec<ReferenceTypeMethodsReceiveDeclared>, String> {
    let JDWPPacketDataFromDebuggee::ReferenceTypeMethods(ReferenceTypeMethodsReceive {
      declared: methods,
    }) = self
      .send_and_receive(&JDWPPacketDataFromDebugger::ReferenceTypeMethods(
        ReferenceTypeMethodsSend {
          ref_type: class_id.clone(),
        },
      ))
      .await?
    else {
      return Err("Unexpected reply to ReferenceType.Methods".into());
    };
    Ok(methods)
  }

  // InvokeMethod 中は全スレッドが動き GC が起こりうるため、複数の呼び出しをまたいで使うオブジェクトは回収を止める
  async fn disable_collection(&mut self, object: &JDWPIDLengthEqObject) -> Result<(), String> {
    self
      .send_and_receive(
        &JDWPPacketDataFromDebugger::ObjectReferenceDisableCollection(
          ObjectReferenceDisableCollectionSend {
            object: object.clone(),
          },
        ),
      )
      .await?;
    Ok(())
  }

  async fn enable_collection(&mut self, object: &JDWPIDLengthEqObject) -> Result<(), String> {
    self
      .send_and_receive(
        &JDWPPacketDataFromDebugger::ObjectReferenceEnableCollection(
          ObjectReferenceEnableCollectionSend {
            object: object.clone(),
          },
        ),
      )
      .await?;
    Ok(())
  }

  async fn invoke_class_method_return_object(
    &mut self,
    clazz: &JDWPIDLengthEqReferenceType,
    method_id: &JDWPIDLengthEqMethod,
    thread: &JDWPIDLengthEqObject,
    args: &[JDWPValue],
  ) -> Result<JDWPIDLengthEqObject, String> {
    let JDWPPacketDataFromDebuggee::ClassTypeInvokeMethod(ClassTypeInvokeMethodReceive {
      return_value,
      exception,
    }) = self
      .send_and_receive(&JDWPPacketDataFromDebugger::ClassTypeInvokeMethod(
        ClassTypeInvokeMethodSend {
          clazz: clazz.clone(),
          thread: thread.clone(),
          method_id: method_id.clone(),
          arguments: args
            .iter()
            .map(|arg| ClassTypeInvokeMethodSendArguments { arg: arg.clone() })
            .collect(),
          options: 0,
        },
      ))
      .await?
    else {
      return Err("Unexpected reply to ClassType.InvokeMethod".into());
    };

    self.check_exception(&exception, thread).await?;
    expect_object(return_value)
  }

  async fn invoke_object_method_return_object(
    &mut self,
    clazz: &JDWPIDLengthEqReferenceType,
    object: &JDWPIDLengthEqObject,
    method_id: &JDWPIDLengthEqMethod,
    thread: &JDWPIDLengthEqObject,
    args: &[JDWPValue],
  ) -> Result<JDWPIDLengthEqObject, String> {
    let ObjectReferenceInvokeMethodReceive {
      return_value,
      exception,
    } = self
      .invoke_object_method(clazz, object, method_id, thread, args)
      .await?;

    self.check_exception(&exception, thread).await?;
    expect_object(return_value)
  }

  // 例外の確認をしない版。check_exception 自身が例外の toString() を呼ぶときに再帰しないよう分けている
  async fn invoke_object_method(
    &mut self,
    clazz: &JDWPIDLengthEqReferenceType,
    object: &JDWPIDLengthEqObject,
    method_id: &JDWPIDLengthEqMethod,
    thread: &JDWPIDLengthEqObject,
    args: &[JDWPValue],
  ) -> Result<ObjectReferenceInvokeMethodReceive, String> {
    let JDWPPacketDataFromDebuggee::ObjectReferenceInvokeMethod(received) = self
      .send_and_receive(&JDWPPacketDataFromDebugger::ObjectReferenceInvokeMethod(
        ObjectReferenceInvokeMethodSend {
          object: object.clone(),
          clazz: clazz.clone(),
          thread: thread.clone(),
          method_id: method_id.clone(),
          arguments: args
            .iter()
            .map(|arg| ObjectReferenceInvokeMethodSendArguments { arg: arg.clone() })
            .collect(),
          options: 0,
        },
      ))
      .await?
    else {
      return Err("Unexpected reply to ObjectReference.InvokeMethod".into());
    };
    Ok(received)
  }

  async fn new_instance(
    &mut self,
    clazz: &JDWPIDLengthEqReferenceType,
    constructor: &JDWPIDLengthEqMethod,
    thread: &JDWPIDLengthEqObject,
    args: &[JDWPValue],
  ) -> Result<JDWPIDLengthEqObject, String> {
    let JDWPPacketDataFromDebuggee::ClassTypeNewInstance(ClassTypeNewInstanceReceive {
      new_object,
      exception,
    }) = self
      .send_and_receive(&JDWPPacketDataFromDebugger::ClassTypeNewInstance(
        ClassTypeNewInstanceSend {
          clazz: clazz.clone(),
          thread: thread.clone(),
          method_id: constructor.clone(),
          arguments: args
            .iter()
            .map(|arg| ClassTypeNewInstanceSendArguments { arg: arg.clone() })
            .collect(),
          options: 0,
        },
      ))
      .await?
    else {
      return Err("Unexpected reply to ClassType.NewInstance".into());
    };

    self.check_exception(&exception, thread).await?;
    Ok(new_object.object_id.into())
  }

  async fn check_exception(
    &mut self,
    exception: &JDWPTaggedObjectID,
    thread: &JDWPIDLengthEqObject,
  ) -> Result<(), String> {
    if exception.object_id == 0 {
      return Ok(());
    }
    let description = match self.get_exception_string(exception, thread).await {
      Ok(s) => s,
      // 例外の表示に失敗しても、例外が起きたこと自体は伝える
      Err(e) => format!(
        "(object id {}; failed to describe: {})",
        exception.object_id, e
      ),
    };
    Err(format!("Exception thrown in JVM: {}", description))
  }

  // getMessage() は null を返しうるため、例外クラス名も含めて null にならない toString() を使う
  async fn get_exception_string(
    &mut self,
    exception: &JDWPTaggedObjectID,
    thread: &JDWPIDLengthEqObject,
  ) -> Result<String, String> {
    let Some(throwable) = self.throwable.clone() else {
      return Err("java.lang.Throwable is not resolved yet".into());
    };

    let ObjectReferenceInvokeMethodReceive {
      return_value,
      exception: exception_in_to_string,
    } = self
      .invoke_object_method(
        &throwable.clazz,
        &exception.object_id.into(),
        &throwable.to_string,
        thread,
        &[],
      )
      .await?;

    if exception_in_to_string.object_id != 0 {
      return Err("toString() threw an exception".into());
    }
    let string_object = expect_object(return_value)?;
    self.get_string_value(&string_object).await
  }

  async fn calc_expression(
    &mut self,
    expr: &str,
    big_integer: &BigIntegerMethods,
    current_thread: &JDWPIDLengthEqObject,
  ) -> Result<String, String> {
    let exprs = parse::parse_input(expr).map_err(|e| format!("Parse error: {}", e))?;

    let mut collection_disabled = Vec::new();
    let result = self
      .eval_expressions(exprs, big_integer, current_thread, &mut collection_disabled)
      .await;

    // 回収の再開に失敗しても計算結果には影響しないため、エラーは無視する
    for object in &collection_disabled {
      let _ = self.enable_collection(object).await;
    }

    result
  }

  async fn eval_expressions(
    &mut self,
    exprs: Vec<parse::Expression>,
    big_integer: &BigIntegerMethods,
    current_thread: &JDWPIDLengthEqObject,
    collection_disabled: &mut Vec<JDWPIDLengthEqObject>,
  ) -> Result<String, String> {
    let h = self;
    let log = h.log;

    let mut stack: Vec<JDWPIDLengthEqObject> = Vec::new();
    for expr in exprs {
      let result = match expr {
        parse::Expression::Number(n) => {
          log.doing(&format!("Invoking new BigInteger(\"{}\")", n));
          let string = h.load_string(&n).await?;
          h.new_instance(
            &big_integer.clazz,
            &big_integer.new_from_string,
            current_thread,
            &[JDWPValue::Object(string)],
          )
          .await?
        }
        parse::Expression::Negate => {
          let a = stack.pop().expect("Stack underflow");
          log.doing(&format!("Invoke: negate {}", a));
          h.invoke_object_method_return_object(
            &big_integer.clazz,
            &a,
            &big_integer.negate,
            current_thread,
            &[],
          )
          .await?
        }
        parse::Expression::Binary(op) => {
          let b = stack.pop().expect("Stack underflow");
          let a = stack.pop().expect("Stack underflow");
          log.doing_ln(&format!("Calc binary expression: {} {:?} {}", a, op, b));
          let op_method = match op {
            parse::Operator::Add => &big_integer.add,
            parse::Operator::Subtract => &big_integer.subtract,
            parse::Operator::Multiply => &big_integer.multiply,
            parse::Operator::Divide => &big_integer.divide,
          };

          log.doing(&format!("Invoke: {:?}", op));
          h.invoke_object_method_return_object(
            &big_integer.clazz,
            &a,
            op_method,
            current_thread,
            &[JDWPValue::Object(b)],
          )
          .await?
        }
      };
      h.disable_collection(&result).await?;
      collection_disabled.push(result.clone());
      stack.push(result);
      log.done();
    }

    log.doing("Result obtained. call toString()");
    let result_bigint = stack.pop().expect("Stack underflow");
    let result_string_obj = h
      .invoke_object_method_return_object(
        &big_integer.clazz,
        &result_bigint,
        &big_integer.to_string,
        current_thread,
        &[],
      )
      .await?;
    log.done();

    log.doing("Get string value");
    let result = h.get_string_value(&result_string_obj).await?;
    log.done();

    Ok(result)
  }
}

fn is_vm_death(event_composite: &EventCompositeReceive) -> bool {
  event_composite.events.iter().any(|event| {
    matches!(
      event.event_kind,
      EventCompositeReceiveEventsEventKind::_VMDEATH(_)
    )
  })
}

fn find_method(
  methods: &[ReferenceTypeMethodsReceiveDeclared],
  method_name: &str,
  signature: &str,
) -> Result<JDWPIDLengthEqMethod, String> {
  methods
    .iter()
    .find(|method| method.name.data == method_name && method.signature.data == signature)
    .map(|method| method.method_id.clone())
    .ok_or_else(|| format!("Method {}{} not found", method_name, signature))
}

// null は呼び出し側で使えないため、オブジェクト以外と同じくエラーにする
fn expect_object(value: JDWPValue) -> Result<JDWPIDLengthEqObject, String> {
  match value {
    JDWPValue::Object(obj_id)
    | JDWPValue::Array(obj_id)
    | JDWPValue::ClassObject(obj_id)
    | JDWPValue::String(obj_id) => {
      if obj_id.id == 0 {
        Err("Method returned null".into())
      } else {
        Ok(obj_id)
      }
    }
    _ => Err("Expected object return value".into()),
  }
}
