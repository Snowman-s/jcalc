*I requested the AI to generate a README containing wildly exaggerated descriptions.*

# 🧮 JVM Delegated Calculator

*「1 + 1 を直接計算するなんて、20世紀のやり方だ。」*

---

## 概要

**JVM Delegated Calculator** は、  
ローカルで実行中（または停止中）の JVM に対して  
**デバッグプロトコル (Java Debug Wire Protocol; JDWP)** 経由で計算を委譲する  
世界初（たぶん）の電卓アプリケーションです。

通常の電卓はプロセス内で直接 CPU 命令を発行します。  
しかし本プロジェクトは、敢えて次のような手順を踏みます：

1. ユーザーが `1 + 1` を入力する  
2. アプリがローカルホスト上の JVM にデバッグ接続  
3. JVM に `BigInteger.valueOf(1).add(BigInteger.valueOf(1))` の実行を「お願い」する (※)
4. JVM が結果 `2` を返す（慈悲深く）  

こうしてあなたの手元に戻る「2」は、  
**仮想マシンによって正統に認定された加算結果** です。

---

## 特徴

- 🧘 **ローカル非効率最適化**  
  同一マシン内でわざわざプロトコル通信を行うことで、  
  「非効率」を極限まで形式化します。

- 🐢 **Debug Driven Arithmetic™**  
  計算はすべて JDWP 経由でデバッグ命令として発行。  
  計算結果は、**実行中スレッドの停止を伴う真剣勝負** です。

- 🔄 **同一マシン分散コンピューティング**  
  クラウドを使わずとも、あなたの CPU 内に“距離”を生み出せます。

- 🧩 **哲学的ローカル性**  
  「ローカル実行」とは何か？  
  ― このプロジェクトが再定義します。

---

## 使い方

### 0. リポジトリをclone
```terminal
git clone --recurse-submodules https://github.com/Snowman-s/jcalc.git
cd jcalc
```

### 1. JVM を用意する
```terminal
$cd java
$javac Main.java
$java -agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=*:5005 Main
```

【重要！】**ソースファイルは Main.java というファイル名でなければなりません。**

JVM は**停止状態 (suspend=y)** で待機してください。  
この神聖な儀式を経ずに計算を始めてはいけません。

### 2. 電卓を起動する
```
$ cd jcalc
$ cargo run -- -v 
```

`-v` を付けると現在実行中の処理を出します (過剰なほどに)


### 3. 計算を依頼する
```
> 1 + 1
* Constructing Long from 1..OK!
* Creating JVM array for Long to invoke BigInteger.valueOf..OK!
* Invoking BigInteger.valueOf..OK!
* Constructing Long from 1..OK!
* Creating JVM array for Long to invoke BigInteger.valueOf..OK!
* Invoking BigInteger.valueOf..OK!
* Calc binary expression: Int(36) Add Int(36)..
* Creating JVM array for BigInteger operation Add..OK!
* Invoke: JDWPIDLengthEqObject { id: 20 }..OK!
* Result obtained. call toString()..OK!
* Get string value..OK!
= 2
```

---

## なぜ？

> “なぜ直接計算しないのか？”  
> ――それは可能だからだ。

このプロジェクトは、  
**「計算」という行為をいかに過剰に仮想化できるか**  
という人類の挑戦に対する一つの回答です。

---

## サポートされている演算

四則演算と括弧がサポートされています。

例: 
```
jcalc> (10 + 30) * 3 / 5
= 24
```

---

(※) 1 + 1 のとき、実際には以下が発行されます。

```java
Class bigIntClass = Class.forName("java.math.BigInteger");
Method bigIntValueOf = bigIntClass.getMethod("valueOf", new Class[] { Long.TYPE });
Method bigIntAdd = bigIntClass.getMethod("add", new Class[] { bigIntClass });
Method bigIntToString = bigIntClass.getMethod("toString", (Class[]) null);

Object a = bigIntValueOf.invoke(null, new Object[] { Long.valueOf(1) });
Object b = bigIntValueOf.invoke(null, new Object[] { Long.valueOf(1) });

Object sum = bigIntAdd.invoke(a, new Object[] { b });

Object answer = bigIntToString.invoke(sum, (Object[]) null); // 結果
```
