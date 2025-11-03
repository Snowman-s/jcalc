use std::io::Write;
use std::io::stderr;
use std::io::stdout;
use std::sync::Arc;
use std::vec;

use clap::Parser;
use futures_util::lock::Mutex;
use ore_jdwp::defs::ArrayReferenceSetValuesSend;
use ore_jdwp::defs::ArrayReferenceSetValuesSendValues;
use ore_jdwp::defs::ArrayTypeNewInstanceReceive;
use ore_jdwp::defs::ArrayTypeNewInstanceSend;
use ore_jdwp::defs::ClassTypeInvokeMethodReceive;
use ore_jdwp::defs::ClassTypeInvokeMethodSend;
use ore_jdwp::defs::ClassTypeInvokeMethodSendArguments;
use ore_jdwp::defs::EventCompositeReceiveEventsEventKind;
use ore_jdwp::defs::EventRequestSetSend;
use ore_jdwp::defs::EventRequestSetSendModifiers;
use ore_jdwp::defs::EventRequestSetSendModifiersModKind;
use ore_jdwp::defs::EventRequestSetSendModifiersModKind12;
use ore_jdwp::defs::ObjectReferenceInvokeMethodReceive;
use ore_jdwp::defs::ObjectReferenceInvokeMethodSend;
use ore_jdwp::defs::ObjectReferenceInvokeMethodSendArguments;
use ore_jdwp::defs::ReferenceTypeFieldsReceive;
use ore_jdwp::defs::ReferenceTypeFieldsSend;
use ore_jdwp::defs::ReferenceTypeGetValuesReceive;
use ore_jdwp::defs::ReferenceTypeGetValuesSend;
use ore_jdwp::defs::ReferenceTypeGetValuesSendFields;
use ore_jdwp::defs::ReferenceTypeMethodsReceive;
use ore_jdwp::defs::ReferenceTypeMethodsSend;
use ore_jdwp::defs::StringReferenceValueReceive;
use ore_jdwp::defs::StringReferenceValueSend;
use ore_jdwp::defs::VirtualMachineAllThreadsReceive;
use ore_jdwp::defs::VirtualMachineClassesBySignatureReceive;
use ore_jdwp::defs::VirtualMachineClassesBySignatureSend;
use ore_jdwp::defs::VirtualMachineCreateStringReceive;
use ore_jdwp::defs::VirtualMachineCreateStringSend;
use ore_jdwp::packets::ConvPrettyIOValue;
use ore_jdwp::packets::JDWPIDLengthEqField;
use ore_jdwp::packets::JDWPIDLengthEqMethod;
use ore_jdwp::packets::JDWPIDLengthEqObject;
use ore_jdwp::packets::JDWPIDLengthEqReferenceType;
use ore_jdwp::packets::JDWPValue;
use ore_jdwp::packets::PrettyIOKind;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use ore_jdwp::packets::{JDWPContext, JDWPPacketDataFromDebuggee, JDWPPacketDataFromDebugger};
use ore_jdwp::packets::{receive_packet, send_packet};

mod parse;

#[derive(Parser, Debug)]
#[command(name = "tcp_client")]
struct Args {
  /// Host to connect to
  #[arg(
    short = 'H',
    long,
    default_value = "127.0.0.1",
    help = "Target jvm host to connect to"
  )]
  host: String,

  /// Port to connect to
  #[arg(
    short,
    long,
    default_value = "5005",
    help = "Target jvm port to connect to"
  )]
  port: String,

  #[arg(short, long, default_value = "false", help = "Enable verbose output")]
  verbose: bool,

  #[arg(
    short,
    long,
    default_value = "Main.java",
    help = "Source file including main method"
  )]
  source_file: String,

  #[arg(short, long, help = "If set, calc desinated expression and exit")]
  expression: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let args = Args::parse();
  let addr = format!("{}:{}", args.host, args.port);

  let mut stream = TcpStream::connect(addr.clone()).await?;
  if args.verbose {
    eprintln!("Connected to {}", addr);
  }

  let payloads: Arc<Mutex<Vec<JDWPPacketDataFromDebugger>>> = Arc::new(Mutex::new(Vec::new()));
  let context = Arc::new(Mutex::new(JDWPContext { id_sizes: None }));

  // --- Handshake ---
  let handshake = b"JDWP-Handshake";
  stream.write_all(handshake).await?;
  stream.flush().await?;
  if args.verbose {
    eprintln!("Sent handshake: {:?}", String::from_utf8_lossy(handshake));
  }

  // 応答を読む（同期的に一度読む）
  let mut buf = [0u8; 14];
  stream.read_exact(&mut buf).await?;
  if &buf != b"JDWP-Handshake" {
    eprintln!("Invalid handshake response");
    return Err(Box::from("Invalid handshake response"));
  }
  if args.verbose {
    eprintln!("Handshake successful!");
  }

  // --- ここから非同期で送受信を分離 ---
  let (reader, writer) = stream.into_split();
  // 受信スレッドから送信スレッドへのチャネル
  let (channel_tx, channel_rx) = mpsc::channel::<JDWPPacketDataFromDebuggee>(8192);

  // payload の保存用
  let payloads_recv = Arc::clone(&payloads);
  let context_recv = Arc::clone(&context);
  let payloads_send = Arc::clone(&payloads);

  // 受信タスク
  let recv_task = tokio::spawn(handle_receive(
    reader,
    payloads_recv,
    context_recv,
    channel_tx,
  ));

  // 送信タスク
  let send_task = tokio::spawn(handle_send(
    writer,
    payloads_send,
    context,
    channel_rx,
    args.verbose,
    args,
  ));

  let (_recv_result, send_result) = tokio::try_join!(recv_task, send_task)?;
  if send_result.is_err() {
    eprintln!("Error in send task: {}", send_result.err().unwrap());
  }
  Ok(())
}

async fn handle_receive(
  mut reader: tokio::net::tcp::OwnedReadHalf,
  payloads: Arc<Mutex<Vec<JDWPPacketDataFromDebugger>>>,
  context: Arc<Mutex<JDWPContext>>,
  channel_tx: mpsc::Sender<JDWPPacketDataFromDebuggee>,
) {
  while let Ok(length) = reader.read_u32().await {
    let mut buf = vec![0u8; length as usize - 4];

    let n = reader.read_exact(&mut buf).await.unwrap();

    // Await the async receive_packet function
    let packet_and_id = receive_packet(
      length as usize - 4,
      &mut &buf[..n],
      &payloads.lock().await,
      &*context.lock().await,
    )
    .await;

    // FIXME: まだContextが取得できておらず、エラーでもあるなら、捨てる
    {
      if context.lock().await.id_sizes.is_none() && packet_and_id.is_none() {
        continue;
      }
    }

    if packet_and_id.is_none() {
      eprint!("\n\nReceived packet: ");
      if n > 256 {
        eprint!("(too long to display) ");
      } else {
        for b in &buf[..n] {
          eprint!("{:02X} ", b);
        }
      }
      eprintln!();
      let id = u32::from_be_bytes(buf[0..4].try_into().unwrap());
      eprintln!("Send: {:?}", payloads.lock().await[id as usize]);
      stderr().flush().unwrap();
      panic!("Failed to decode packet")
    }

    let (packet, _) = packet_and_id.unwrap();

    channel_tx.send(packet).await.unwrap();
  }
}

async fn handle_send(
  writer: tokio::net::tcp::OwnedWriteHalf,
  payloads: Arc<Mutex<Vec<JDWPPacketDataFromDebugger>>>,
  context: Arc<Mutex<JDWPContext>>,
  channel_rx: mpsc::Receiver<JDWPPacketDataFromDebuggee>,
  verbose: bool,
  args: Args,
) -> Result<(), String> {
  let Args {
    source_file,
    expression,
    ..
  } = args;

  let print_ln_what_is_doing = |what: &str| {
    if verbose {
      eprintln!("* {}..", what);
    }
  };
  let print_what_is_doing = |what: &str| {
    if verbose {
      eprint!("* {}", what);
    }
  };
  let print_done = || {
    if verbose {
      eprintln!("..OK!");
    }
  };
  let print_info = |info: &str| {
    if verbose {
      eprintln!("* {}", info);
    }
  };

  let mut h = SendHandler {
    writer,
    payloads,
    context,
    channel_rx,
    cmd_id: 0,
  };

  print_what_is_doing("Get id sizes");
  h.get_id_sizes().await?;
  print_done();

  // main() メソッドを待つ
  print_what_is_doing("Set method entry breakpoint");
  h.send_and_receive(&JDWPPacketDataFromDebugger::EventRequestSet(
    EventRequestSetSend {
      suspend_policy: 2,
      modifiers: vec![EventRequestSetSendModifiers {
        mod_kind: EventRequestSetSendModifiersModKind::_12(EventRequestSetSendModifiersModKind12 {
          source_name_pattern: source_file.as_str().into(),
        }),
      }],
      event_kind: 8, // PrepareClass
    },
  ))
  .await?;
  print_done();

  // 最初の停止まで実行
  print_what_is_doing("Resume VM");
  h.send_and_receive(&JDWPPacketDataFromDebugger::VirtualMachineResume(()))
    .await?;
  print_done();

  // 停止待ち
  print_what_is_doing("Wait for breakpoint hit");
  loop {
    let packet = h.channel_rx.recv().await.unwrap();
    if let JDWPPacketDataFromDebuggee::EventComposite(event_composite) = packet {
      if event_composite.events.iter().any(|event| {
        matches!(
          event.event_kind,
          EventCompositeReceiveEventsEventKind::_CLASSPREPARE(_)
        )
      }) {
        break;
      }
    }
  }
  print_done();

  // 現在のスレッドIDを取得する
  print_what_is_doing("Find current thread");
  let JDWPPacketDataFromDebuggee::VirtualMachineAllThreads(VirtualMachineAllThreadsReceive {
    threads,
  }) = h
    .send_and_receive(&JDWPPacketDataFromDebugger::VirtualMachineAllThreads(()))
    .await?
  else {
    panic!("Failed to get all threads")
  };
  let current_thread = threads.first().expect("No thread found").thread.clone();
  print_done();
  print_info(&format!("Current thread id: {}", current_thread));

  // Class の id を問い合わせる
  print_what_is_doing("Find java.lang.Class");
  let clazz_of_class = h
    .find_class("Ljava/lang/Class;")
    .await
    .expect("Failed to find Class class");
  print_done();
  // forName()
  print_what_is_doing("Find Class.forName");
  let method_class_for_name = h
    .find_method(
      &clazz_of_class,
      "forName",
      "(Ljava/lang/String;)Ljava/lang/Class;",
    )
    .await?;
  print_done();
  // getMethod()
  print_what_is_doing("Find Class.getMethod");
  let method_get_method = h
    .find_method(
      &clazz_of_class,
      "getMethod",
      "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
    )
    .await?;
  print_done();
  // Long の id を得る
  print_what_is_doing("Find java.lang.Long");
  let clazz_long = h
    .find_class("Ljava/lang/Long;")
    .await
    .expect("Failed to find Long class");
  print_done();
  // Long.valueOf(long) を得る
  print_what_is_doing("Find Long.valueOf");
  let method_long_value_of = h
    .find_method(&clazz_long, "valueOf", "(J)Ljava/lang/Long;")
    .await?;
  print_done();
  // java.lang.Long.TYPE フィールドの取得
  print_what_is_doing("Find Long.TYPE");
  let field_long_type = h
    .find_field(&clazz_long, "TYPE", "Ljava/lang/Class;")
    .await?;
  print_done();

  // Long.TYPE フィールドの値を取得して Class オブジェクトを得る
  print_what_is_doing("Get Long.TYPE value");
  let class_long = {
    let JDWPPacketDataFromDebuggee::ReferenceTypeGetValues(ReferenceTypeGetValuesReceive {
      values,
    }) = h
      .send_and_receive(&JDWPPacketDataFromDebugger::ReferenceTypeGetValues(
        ReferenceTypeGetValuesSend {
          ref_type: clazz_long.clone(),
          fields: vec![ReferenceTypeGetValuesSendFields {
            field_id: field_long_type.clone(),
          }],
        },
      ))
      .await?
    else {
      panic!("Failed to get methods")
    };
    match values
      .first()
      .ok_or("Failed to get Long.TYPE field value")?
      .value
      .clone()
    {
      JDWPValue::Object(obj_id) => obj_id,
      JDWPValue::ClassObject(obj_id) => obj_id,
      _ => return Err("Expected ClassObject value".into()),
    }
  };
  print_done();

  //Class.forName("java.math.BigInteger") を呼び出して BigInteger クラスのIDを得る
  print_what_is_doing("Find java.math.BigInteger");
  let string_big_integer = h.load_string("java.math.BigInteger").await.unwrap();
  let class_big_integer = h
    .invoke_class_method_return_object(
      &clazz_of_class,
      &method_class_for_name,
      &current_thread,
      &[JDWPValue::Object(string_big_integer)],
    )
    .await?;
  print_done();

  // 各メソッドのMethodインスタンスのメソッドIDを得る
  print_what_is_doing("Find BigInteger.valueOf");
  let value_of_method_instance = {
    let name = h.load_string("valueOf").await?;
    let arg = h
      .create_jvm_array_from_jdwpvalues(
        "[Ljava/lang/Class;",
        vec![JDWPValue::ClassObject(class_long.clone())],
      )
      .await?;
    h.invoke_object_method_return_object(
      &clazz_of_class,
      &class_big_integer.clone(),
      &method_get_method,
      &current_thread,
      &[JDWPValue::String(name), JDWPValue::Array(arg)],
    )
    .await?
  };
  print_done();

  print_what_is_doing("Find BigInteger add methods");
  let add_method_instance = {
    let name = h.load_string("add").await?;
    let arg = h
      .create_jvm_array_from_jdwpvalues(
        "[Ljava/lang/Class;",
        vec![JDWPValue::ClassObject(class_big_integer.clone())],
      )
      .await?;
    h.invoke_object_method_return_object(
      &clazz_of_class,
      &class_big_integer.clone(),
      &method_get_method,
      &current_thread,
      &[JDWPValue::String(name), JDWPValue::Array(arg)],
    )
    .await?
  };
  print_done();

  print_what_is_doing("Find BigInteger subtract methods");
  let subtract_method_instance = {
    let name = h.load_string("subtract").await?;
    let arg = h
      .create_jvm_array_from_jdwpvalues(
        "[Ljava/lang/Class;",
        vec![JDWPValue::ClassObject(class_big_integer.clone())],
      )
      .await?;
    h.invoke_object_method_return_object(
      &clazz_of_class,
      &class_big_integer.clone(),
      &method_get_method,
      &current_thread,
      &[JDWPValue::String(name), JDWPValue::Array(arg)],
    )
    .await?
  };
  print_done();

  print_what_is_doing("Find BigInteger multiply methods");
  let multiply_method_instance = {
    let name = h.load_string("multiply").await?;
    let arg = h
      .create_jvm_array_from_jdwpvalues(
        "[Ljava/lang/Class;",
        vec![JDWPValue::ClassObject(class_big_integer.clone())],
      )
      .await?;
    h.invoke_object_method_return_object(
      &clazz_of_class,
      &class_big_integer.clone(),
      &method_get_method,
      &current_thread,
      &[JDWPValue::String(name), JDWPValue::Array(arg)],
    )
    .await?
  };
  print_done();

  print_what_is_doing("Find BigInteger divide methods");
  let divide_method_instance = {
    let name = h.load_string("divide").await?;
    let arg = h
      .create_jvm_array_from_jdwpvalues(
        "[Ljava/lang/Class;",
        vec![JDWPValue::ClassObject(class_big_integer.clone())],
      )
      .await?;
    h.invoke_object_method_return_object(
      &clazz_of_class,
      &class_big_integer.clone(),
      &method_get_method,
      &current_thread,
      &[JDWPValue::String(name), JDWPValue::Array(arg)],
    )
    .await?
  };
  print_done();

  print_what_is_doing("Find BigInteger toString methods");
  let to_string_method_instance = {
    let name = h.load_string("toString").await?;
    h.invoke_object_method_return_object(
      &clazz_of_class,
      &class_big_integer.clone(),
      &method_get_method,
      &current_thread,
      &[
        JDWPValue::String(name),
        JDWPValue::Array(
          JDWPIDLengthEqObject::from_value(&vec![PrettyIOKind::Int(0)])
            .unwrap()
            .0,
        ),
      ],
    )
    .await?
  };
  print_done();

  // Method クラスを得る
  print_what_is_doing("Find java.lang.reflect.Method");
  let clazz_method = h.find_class("Ljava/lang/reflect/Method;").await?;
  print_done();

  print_what_is_doing("Find Method.invoke");
  let invoke_method = h
    .find_method(
      &clazz_method,
      "invoke",
      "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
    )
    .await?;
  print_done();

  let mut input = String::new();
  let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());

  if let Some(ref expr) = expression {
    match h
      .calc_expression(
        expr,
        &clazz_long,
        &method_long_value_of,
        &clazz_method,
        &value_of_method_instance,
        &add_method_instance,
        &subtract_method_instance,
        &multiply_method_instance,
        &divide_method_instance,
        &to_string_method_instance,
        &invoke_method,
        &current_thread,
        &Box::new(print_what_is_doing),
        &Box::new(print_ln_what_is_doing),
        &Box::new(print_done),
      )
      .await
    {
      Ok(result) => {
        print!("{}", result);
      }
      Err(e) => {
        return Err(format!("Parse error: {}", e));
      }
    }
  } else if atty::is(atty::Stream::Stdin) {
    loop {
      print!("jcalc> ");
      stdout().flush().unwrap();
      stdin.read_line(&mut input).await.unwrap();
      if input.trim() == "exit" {
        break;
      }

      match h
        .calc_expression(
          &input,
          &clazz_long,
          &method_long_value_of,
          &clazz_method,
          &value_of_method_instance,
          &add_method_instance,
          &subtract_method_instance,
          &multiply_method_instance,
          &divide_method_instance,
          &to_string_method_instance,
          &invoke_method,
          &current_thread,
          &Box::new(print_what_is_doing),
          &Box::new(print_ln_what_is_doing),
          &Box::new(print_done),
        )
        .await
      {
        Ok(result) => {
          println!("{}", result);
        }
        Err(e) => {
          eprintln!("Parse error: {}", e);
        }
      }
    }
  } else {
    let mut expr: String = String::new();
    stdin.read_line(&mut expr).await.unwrap();

    match h
      .calc_expression(
        &expr,
        &clazz_long,
        &method_long_value_of,
        &clazz_method,
        &value_of_method_instance,
        &add_method_instance,
        &subtract_method_instance,
        &multiply_method_instance,
        &divide_method_instance,
        &to_string_method_instance,
        &invoke_method,
        &current_thread,
        &Box::new(print_what_is_doing),
        &Box::new(print_ln_what_is_doing),
        &Box::new(print_done),
      )
      .await
    {
      Ok(result) => {
        print!("{}", result);
      }
      Err(e) => {
        return Err(format!("Parse error: {}", e));
      }
    }
  }
  Ok(())
}

struct SendHandler {
  writer: tokio::net::tcp::OwnedWriteHalf,
  payloads: Arc<Mutex<Vec<JDWPPacketDataFromDebugger>>>,
  context: Arc<Mutex<JDWPContext>>,
  channel_rx: mpsc::Receiver<JDWPPacketDataFromDebuggee>,
  cmd_id: i32,
}

impl SendHandler {
  async fn send_and_receive(
    &mut self,
    payload: &JDWPPacketDataFromDebugger,
  ) -> Result<JDWPPacketDataFromDebuggee, String> {
    // Clone the payload to avoid borrowing issues
    let payload_clone = payload.clone();

    // Send the packet synchronously using block_on or similar approach
    {
      self.payloads.lock().await.push(payload_clone.clone());
      send_packet(&mut self.writer, self.cmd_id, &payload_clone)
        .await
        .unwrap();
      self.cmd_id += 1;
    }

    loop {
      match self.channel_rx.recv().await {
        Some(JDWPPacketDataFromDebuggee::EventComposite(event_composite)) => {
          if event_composite.events.iter().any(|event| {
            matches!(
              event.event_kind,
              EventCompositeReceiveEventsEventKind::_VMDEATH(_)
            )
          }) {
            return Err("VM DEATH".into());
          }
        }
        Some(response_packet) => {
          println!("! {:?} -> {:?}", payload_clone, response_packet);
          return Ok(response_packet);
        }
        None => {
          return Err("Channel closed".into());
        }
      }
    }
  }

  async fn get_id_sizes(&mut self) -> Result<(), String> {
    let JDWPPacketDataFromDebuggee::VirtualMachineIDSizes(id_sizes) = self
      .send_and_receive(&JDWPPacketDataFromDebugger::VirtualMachineIDSizes(()))
      .await?
    else {
      panic!("Failed to get id sizes")
    };
    self
      .context
      .lock()
      .await
      .set_from_id_sizes_response(&id_sizes);

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
      panic!("Failed to create string")
    };
    Ok(str.clone())
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
      panic!("Failed to find class")
    };
    Ok(classes.first().expect("No class found").type_id.clone())
  }

  async fn find_method(
    &mut self,
    class_id: &JDWPIDLengthEqReferenceType,
    method_name: &str,
    signature: &str,
  ) -> Result<JDWPIDLengthEqMethod, String> {
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
      panic!("Failed to get methods")
    };
    for method in methods {
      if method.name.data == method_name && method.signature.data == signature {
        return Ok(method.method_id.clone());
      }
    }
    Err(format!("Method {} not found", method_name))
  }

  async fn find_field(
    &mut self,
    class_id: &JDWPIDLengthEqReferenceType,
    field_name: &str,
    signature: &str,
  ) -> Result<JDWPIDLengthEqField, String> {
    let JDWPPacketDataFromDebuggee::ReferenceTypeFields(ReferenceTypeFieldsReceive {
      declared: fields,
    }) = self
      .send_and_receive(&JDWPPacketDataFromDebugger::ReferenceTypeFields(
        ReferenceTypeFieldsSend {
          ref_type: class_id.clone(),
        },
      ))
      .await?
    else {
      panic!("Failed to get fields")
    };
    for field in fields {
      if field.name.data == field_name && field.signature.data == signature {
        return Ok(field.field_id.clone());
      }
    }
    Err(format!("Field {} not found", field_name))
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
      exception: _exception,
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
      panic!("Failed to invoke method")
    };

    match return_value {
      JDWPValue::Object(obj_id) => Ok(obj_id),
      JDWPValue::ClassObject(obj_id) => Ok(obj_id),
      _ => Err("Expected object return value".into()),
    }
  }

  async fn invoke_object_method_return_object(
    &mut self,
    clazz: &JDWPIDLengthEqReferenceType,
    object: &JDWPIDLengthEqObject,
    method_id: &JDWPIDLengthEqMethod,
    thread: &JDWPIDLengthEqObject,
    args: &[JDWPValue],
  ) -> Result<JDWPIDLengthEqObject, String> {
    let JDWPPacketDataFromDebuggee::ObjectReferenceInvokeMethod(
      ObjectReferenceInvokeMethodReceive {
        return_value,
        exception,
      },
    ) = self
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
      panic!("Failed to invoke method")
    };

    if exception.object_id != 0 {
      return Err(format!(
        "Method invocation threw an exception {}",
        self
          .get_exception_string(
            &JDWPIDLengthEqObject {
              id: exception.object_id
            },
            thread
          )
          .await?,
      ));
    }

    match return_value {
      JDWPValue::Object(obj_id) => Ok(obj_id),
      JDWPValue::Array(obj_id) => Ok(obj_id),
      JDWPValue::ClassObject(obj_id) => Ok(obj_id),
      JDWPValue::String(obj_id) => Ok(obj_id),
      _ => Err("Expected object return value".into()),
    }
  }

  // jdwpvalue の配列を、JVmP の配列オブジェクトに変換するユーティリティ関数
  async fn create_jvm_array_from_jdwpvalues(
    &mut self,
    elements_signature: &str,
    values: Vec<JDWPValue>,
  ) -> Result<JDWPIDLengthEqObject, String> {
    // まず配列のリファレンスを取る
    let clazz_array = self.find_class(elements_signature).await?;

    // 配列オブジェクトの作成
    let JDWPPacketDataFromDebuggee::ArrayTypeNewInstance(ArrayTypeNewInstanceReceive { new_array }) =
      self
        .send_and_receive(&JDWPPacketDataFromDebugger::ArrayTypeNewInstance(
          ArrayTypeNewInstanceSend {
            arr_type: clazz_array.clone(),
            length: values.len() as i32,
          },
        ))
        .await?
    else {
      panic!("Failed to create array")
    };
    let new_array_untagged = JDWPIDLengthEqObject {
      id: new_array.object_id,
    };

    // 配列への値の設定
    self
      .send_and_receive(&JDWPPacketDataFromDebugger::ArrayReferenceSetValues(
        ArrayReferenceSetValuesSend {
          array_object: new_array_untagged.clone(),
          first_index: 0,
          values: values
            .into_iter()
            .map(|v| ArrayReferenceSetValuesSendValues { value: v })
            .collect(),
        },
      ))
      .await?;

    Ok(new_array_untagged)
  }

  async fn get_exception_string(
    &mut self,
    exception: &JDWPIDLengthEqObject,
    thread: &JDWPIDLengthEqObject,
  ) -> Result<String, String> {
    let th = self.find_class("Ljava/lang/Throwable;").await?;
    let get_message_method = self
      .find_method(&th, "getMessage", "()Ljava/lang/String;")
      .await?;

    let JDWPPacketDataFromDebuggee::ObjectReferenceInvokeMethod(
      ObjectReferenceInvokeMethodReceive {
        return_value: JDWPValue::String(return_value),
        exception: _,
      },
    ) = self
      .send_and_receive(&JDWPPacketDataFromDebugger::ObjectReferenceInvokeMethod(
        ObjectReferenceInvokeMethodSend {
          object: exception.clone(),
          clazz: th.clone(),
          thread: thread.clone(),
          method_id: get_message_method.clone(),
          arguments: vec![],
          options: 0,
        },
      ))
      .await?
    else {
      panic!("Failed to invoke method")
    };

    let msg_str = {
      let JDWPPacketDataFromDebuggee::StringReferenceValue(StringReferenceValueReceive {
        string_value,
      }) = self
        .send_and_receive(&JDWPPacketDataFromDebugger::StringReferenceValue(
          StringReferenceValueSend {
            string_object: return_value,
          },
        ))
        .await?
      else {
        panic!("Failed to get string value")
      };
      string_value.data
    };

    Ok(msg_str)
  }

  #[allow(clippy::too_many_arguments)]
  async fn calc_expression(
    &mut self,
    expr: &str,
    clazz_long: &JDWPIDLengthEqReferenceType,
    method_long_value_of: &JDWPIDLengthEqMethod,
    clazz_method: &JDWPIDLengthEqReferenceType,
    value_of_method_instance: &JDWPIDLengthEqObject,
    add_method_instance: &JDWPIDLengthEqObject,
    subtract_method_instance: &JDWPIDLengthEqObject,
    multiply_method_instance: &JDWPIDLengthEqObject,
    divide_method_instance: &JDWPIDLengthEqObject,
    to_string_method_instance: &JDWPIDLengthEqObject,
    invoke_method: &JDWPIDLengthEqMethod,
    current_thread: &JDWPIDLengthEqObject,

    print_what_is_doing: impl Fn(&str),
    print_ln_what_is_doing: impl Fn(&str),
    print_done: impl Fn(),
  ) -> Result<String, String> {
    let h = self;

    match parse::parse_input(expr) {
      Ok(exprs) => {
        let mut stack: Vec<JDWPIDLengthEqObject> = Vec::new();
        for expr in exprs {
          match expr {
            parse::Expression::Number(n) => {
              print_what_is_doing(&format!("Constructing Long from {}", n));
              let long_obj = h
                .invoke_class_method_return_object(
                  clazz_long,
                  method_long_value_of,
                  current_thread,
                  &[JDWPValue::Long(n)],
                )
                .await?;
              print_done();

              print_what_is_doing("Creating JVM array for Long to invoke BigInteger.valueOf");
              let arg = h
                .create_jvm_array_from_jdwpvalues(
                  "[Ljava/lang/Object;",
                  vec![JDWPValue::Object(long_obj.clone())],
                )
                .await?;
              print_done();

              print_what_is_doing("Invoking BigInteger.valueOf");
              stack.push(
                h.invoke_object_method_return_object(
                  &clazz_method.clone(),
                  &value_of_method_instance.clone(),
                  &invoke_method.clone(),
                  current_thread,
                  &[
                    JDWPValue::Object(
                      JDWPIDLengthEqObject::from_value(&vec![PrettyIOKind::Int(0)])
                        .unwrap()
                        .0,
                    ),
                    JDWPValue::Array(arg),
                  ],
                )
                .await?,
              );
              print_done();
            }
            parse::Expression::Binary(op) => {
              let b = stack.pop().expect("Stack underflow");
              let a = stack.pop().expect("Stack underflow");
              print_ln_what_is_doing(&format!("Calc binary expression: {} {:?} {}", a, op, b));
              let op_method_instance = {
                match op {
                  parse::Operator::Add => add_method_instance.clone(),
                  parse::Operator::Subtract => subtract_method_instance.clone(),
                  parse::Operator::Multiply => multiply_method_instance.clone(),
                  parse::Operator::Divide => divide_method_instance.clone(),
                }
              };
              print_what_is_doing(&format!(
                "Creating JVM array for BigInteger operation {:?}",
                op
              ));
              let varargs = h
                .create_jvm_array_from_jdwpvalues(
                  "[Ljava/lang/Object;",
                  vec![JDWPValue::Object(b.clone())],
                )
                .await?;
              print_done();

              print_what_is_doing(&format!("Invoke: {:?}", op_method_instance));
              let result = h
                .invoke_object_method_return_object(
                  &clazz_method.clone(),
                  &op_method_instance,
                  invoke_method,
                  current_thread,
                  &[JDWPValue::Object(a), JDWPValue::Array(varargs)],
                )
                .await?;
              stack.push(result);
              print_done();
            }
          }
        }

        print_what_is_doing("Result obtained. call toString()");
        let result_bigint = stack.pop().expect("Stack underflow");
        let result_string_obj = {
          h.invoke_object_method_return_object(
            clazz_method,
            to_string_method_instance,
            invoke_method,
            current_thread,
            &[
              JDWPValue::Object(result_bigint),
              JDWPValue::Array(
                JDWPIDLengthEqObject::from_value(&vec![PrettyIOKind::Int(0)])
                  .unwrap()
                  .0,
              ),
            ],
          )
          .await?
        };
        print_done();

        // 文字列の内容を取得する
        print_what_is_doing("Get string value");
        let JDWPPacketDataFromDebuggee::StringReferenceValue(StringReferenceValueReceive {
          string_value,
        }) = h
          .send_and_receive(&JDWPPacketDataFromDebugger::StringReferenceValue(
            StringReferenceValueSend {
              string_object: result_string_obj,
            },
          ))
          .await?
        else {
          panic!("Failed to get string value")
        };
        print_done();

        Ok(string_value.data)
      }
      Err(e) => Err(e),
    }
  }
}
