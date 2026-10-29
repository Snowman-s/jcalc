#[derive(Debug, PartialEq, Eq)]
pub enum Expression {
  // i64 に収まらない数も受け付けるため、10 進表記のまま保持して JVM 側で BigInteger にする
  Number(String),
  Negate,
  Binary(Operator),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Operator {
  Add,
  Subtract,
  Multiply,
  Divide,
}

// 再帰下降のため、深すぎる入力でスタックオーバーフローしないよう上限を設ける
const MAX_DEPTH: usize = 256;

pub fn parse_input(input: &str) -> Result<Vec<Expression>, String> {
  let mut exprs = Vec::new();
  let remain = parse_add_sub(input, &mut exprs, 0)?;
  if !remain.trim().is_empty() {
    return Err(format!("Unexpected input remaining: '{}'", remain));
  }
  Ok(exprs)
}

// + - のレベル
fn parse_add_sub<'a>(
  input: &'a str,
  exprs: &mut Vec<Expression>,
  depth: usize,
) -> Result<&'a str, String> {
  parse_binary_level(
    input,
    exprs,
    depth,
    |c| match c {
      '+' => Some(Operator::Add),
      '-' => Some(Operator::Subtract),
      _ => None,
    },
    parse_mul_div,
  )
}

// * / のレベル
fn parse_mul_div<'a>(
  input: &'a str,
  exprs: &mut Vec<Expression>,
  depth: usize,
) -> Result<&'a str, String> {
  parse_binary_level(
    input,
    exprs,
    depth,
    |c| match c {
      '*' => Some(Operator::Multiply),
      '/' => Some(Operator::Divide),
      _ => None,
    },
    parse_unary,
  )
}

// 左結合の二項演算子レベルの共通部分
fn parse_binary_level<'a>(
  input: &'a str,
  exprs: &mut Vec<Expression>,
  depth: usize,
  to_operator: fn(char) -> Option<Operator>,
  parse_operand: fn(&'a str, &mut Vec<Expression>, usize) -> Result<&'a str, String>,
) -> Result<&'a str, String> {
  let mut rest = parse_operand(input, exprs, depth)?;
  loop {
    let mut chars = rest.trim_start().chars();
    let Some(op) = chars.next().and_then(to_operator) else {
      return Ok(rest);
    };
    rest = parse_operand(chars.as_str(), exprs, depth)?;
    exprs.push(Expression::Binary(op));
  }
}

// 単項マイナスのレベル。括弧の入れ子も必ずここを通るため、深さの上限はここだけで確認する
fn parse_unary<'a>(
  input: &'a str,
  exprs: &mut Vec<Expression>,
  depth: usize,
) -> Result<&'a str, String> {
  if depth > MAX_DEPTH {
    return Err(format!(
      "Expression is nested too deeply (max {})",
      MAX_DEPTH
    ));
  }
  let s = input.trim_start();
  if let Some(after_minus) = s.strip_prefix('-') {
    let rest = parse_unary(after_minus, exprs, depth + 1)?;
    exprs.push(Expression::Negate);
    Ok(rest)
  } else {
    parse_primary(s, exprs, depth)
  }
}

// 数字や括弧をパース。input は parse_unary で先頭の空白を除去済み
fn parse_primary<'a>(
  input: &'a str,
  exprs: &mut Vec<Expression>,
  depth: usize,
) -> Result<&'a str, String> {
  if let Some(after_paren) = input.strip_prefix('(') {
    let rest = parse_add_sub(after_paren, exprs, depth + 1)?;
    rest
      .trim_start()
      .strip_prefix(')')
      .ok_or_else(|| "Expected ')'".to_string())
  } else {
    let digits_len = input
      .find(|c: char| !c.is_ascii_digit())
      .unwrap_or(input.len());
    if digits_len == 0 {
      if input.is_empty() {
        return Err("Expected number at end of input".to_string());
      }
      return Err(format!("Expected number at '{}'", input));
    }
    exprs.push(Expression::Number(input[..digits_len].to_string()));
    Ok(&input[digits_len..])
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn num(s: &str) -> Expression {
    Expression::Number(s.to_string())
  }

  #[test]
  fn test_parse_expression() {
    let input = "3 + 5 * (2 - 8)";
    let result = parse_input(input);
    assert_eq!(
      result,
      Ok(vec![
        num("3"),
        num("5"),
        num("2"),
        num("8"),
        Expression::Binary(Operator::Subtract),
        Expression::Binary(Operator::Multiply),
        Expression::Binary(Operator::Add),
      ])
    );
  }

  #[test]
  fn test_parse_number_larger_than_i64() {
    assert_eq!(
      parse_input("99999999999999999999"),
      Ok(vec![num("99999999999999999999")])
    );
  }

  #[test]
  fn test_parse_unary_minus() {
    assert_eq!(
      parse_input("2 * -(1 - -3)"),
      Ok(vec![
        num("2"),
        num("1"),
        num("3"),
        Expression::Negate,
        Expression::Binary(Operator::Subtract),
        Expression::Negate,
        Expression::Binary(Operator::Multiply),
      ])
    );
  }

  #[test]
  fn test_parse_minus_binds_tighter_than_mul() {
    assert_eq!(
      parse_input("-2 * 3"),
      Ok(vec![
        num("2"),
        Expression::Negate,
        num("3"),
        Expression::Binary(Operator::Multiply),
      ])
    );
  }

  #[test]
  fn test_parse_empty_input_is_error() {
    assert!(parse_input("").is_err());
    assert!(parse_input("1 +").is_err());
  }

  #[test]
  fn test_parse_too_deep_is_error() {
    let deep_paren = format!("{}1{}", "(".repeat(100_000), ")".repeat(100_000));
    assert!(parse_input(&deep_paren).is_err());
    let deep_minus = format!("{}1", "-".repeat(100_000));
    assert!(parse_input(&deep_minus).is_err());
  }

  #[test]
  fn test_parse_nesting_within_limit() {
    let input = format!("{}1{}", "(".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH));
    assert_eq!(parse_input(&input), Ok(vec![num("1")]));
  }
}
