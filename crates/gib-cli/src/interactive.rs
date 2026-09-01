use dialoguer::{Confirm, Input, Password, Select, console::Term, theme::ColorfulTheme};
use gib::SdkError;
use std::io::{self, BufRead, IsTerminal, Write};

fn theme() -> ColorfulTheme {
    ColorfulTheme {
        prompt_prefix: dialoguer::console::style("◆".to_owned())
            .for_stderr()
            .cyan()
            .bold(),
        prompt_suffix: dialoguer::console::style("›".to_owned())
            .for_stderr()
            .black()
            .bright(),
        success_prefix: dialoguer::console::style("✔".to_owned())
            .for_stderr()
            .green()
            .bold(),
        error_prefix: dialoguer::console::style("✖".to_owned())
            .for_stderr()
            .red()
            .bold(),
        active_item_prefix: dialoguer::console::style("❯".to_owned())
            .for_stderr()
            .cyan()
            .bold(),
        active_item_style: dialoguer::console::Style::new().for_stderr().cyan().bold(),
        ..ColorfulTheme::default()
    }
}

fn rich_terminal() -> Option<Term> {
    let term = Term::stderr();
    (term.is_term() && io::stdin().is_terminal()).then_some(term)
}

fn prompt_error() -> SdkError {
    SdkError::ConfigurationFailure {
        operation: "prompt",
    }
}

pub fn text(prompt: &str) -> Result<String, SdkError> {
    text_with_default(prompt, None, false)
}

pub fn newline() {
    println!();
}

pub fn text_with_default(
    prompt: &str,
    default: Option<&str>,
    allow_empty: bool,
) -> Result<String, SdkError> {
    if let Some(term) = rich_terminal() {
        let theme = theme();
        let mut input = Input::<String>::with_theme(&theme)
            .with_prompt(prompt)
            .allow_empty(allow_empty);
        if let Some(default) = default {
            input = input.default(default.to_owned());
        }
        return input.interact_text_on(&term).map_err(|_| prompt_error());
    }

    loop {
        print_fallback_prompt(prompt, default)?;
        let value = read_line()?;
        if value.is_empty() {
            if let Some(default) = default {
                return Ok(default.to_owned());
            }
            if allow_empty {
                return Ok(value);
            }
            eprintln!(
                "{}",
                dialoguer::console::style("Please enter a value.").yellow()
            );
            continue;
        }
        return Ok(value);
    }
}

pub fn secret(prompt: &str, allow_empty: bool) -> Result<String, SdkError> {
    if let Some(term) = rich_terminal() {
        let theme = theme();
        return Password::with_theme(&theme)
            .with_prompt(prompt)
            .allow_empty_password(allow_empty)
            .interact_on(&term)
            .map_err(|_| prompt_error());
    }

    loop {
        print_fallback_prompt(&format!("{prompt} [hidden]"), None)?;
        let value = read_line()?;
        if !value.is_empty() || allow_empty {
            return Ok(value);
        }
        eprintln!(
            "{}",
            dialoguer::console::style("Please enter a value.").yellow()
        );
    }
}

pub fn select(prompt: &str, options: &[String], default: usize) -> Result<usize, SdkError> {
    if options.is_empty() {
        return Err(SdkError::InvalidRequest {
            field: "selection",
            reason: "at least one option is required",
        });
    }
    let default = default.min(options.len() - 1);
    if let Some(term) = rich_terminal() {
        let theme = theme();
        return Select::with_theme(&theme)
            .with_prompt(prompt)
            .items(options)
            .default(default)
            .interact_on(&term)
            .map_err(|_| prompt_error());
    }

    loop {
        println!();
        println!("{}", dialoguer::console::style(prompt).cyan().bold());
        for (index, option) in options.iter().enumerate() {
            let marker = if index == default { "›" } else { " " };
            println!("  {marker} {}. {option}", index + 1);
        }
        print!("  Choice [{}]: ", default + 1);
        io::stdout().flush().map_err(|_| prompt_error())?;
        let value = read_line()?;
        if value.is_empty() {
            return Ok(default);
        }
        if let Ok(index) = value.parse::<usize>()
            && (1..=options.len()).contains(&index)
        {
            return Ok(index - 1);
        }
        if let Some((index, _)) = options.iter().enumerate().find(|(_, option)| {
            option
                .split_once('—')
                .map_or(option.as_str(), |(short, _)| short)
                .trim()
                .eq_ignore_ascii_case(&value)
        }) {
            return Ok(index);
        }
        eprintln!(
            "{}",
            dialoguer::console::style("Choose one of the displayed options.").yellow()
        );
    }
}

pub fn confirm(prompt: &str, default: bool) -> Result<bool, SdkError> {
    if let Some(term) = rich_terminal() {
        let theme = theme();
        return Confirm::with_theme(&theme)
            .with_prompt(prompt)
            .default(default)
            .interact_on(&term)
            .map_err(|_| prompt_error());
    }

    loop {
        let hint = if default { "Y/n" } else { "y/N" };
        print!("{} [{}]: ", dialoguer::console::style(prompt).cyan(), hint);
        io::stdout().flush().map_err(|_| prompt_error())?;
        let value = read_line()?;
        match value.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => eprintln!(
                "{}",
                dialoguer::console::style("Please answer yes or no.").yellow()
            ),
        }
    }
}

pub fn banner(title: &str, subtitle: &str) {
    let mut lines = vec!["⚡ GIB".to_owned(), single_line(title)];
    if !subtitle.is_empty() {
        lines.push(single_line(subtitle));
    }
    let content_width = lines
        .iter()
        .map(|line| text_width(line))
        .max()
        .unwrap_or(0)
        .max(24);
    let total_width = content_width + 4;

    println!();
    print_box_top("", total_width);
    print_box_row(&lines[0], content_width, true);
    print_box_row(&lines[1], content_width, false);
    if let Some(subtitle) = lines.get(2) {
        print_box_row(subtitle, content_width, false);
    }
    print_box_bottom(total_width);
    println!();
}

pub fn section(title: &str, detail: Option<&str>) {
    println!();
    print!(
        "{} {}",
        dialoguer::console::style("◆").cyan().bold(),
        dialoguer::console::style(title).bold()
    );
    if let Some(detail) = detail {
        print!(
            " {}",
            dialoguer::console::style(format!("· {detail}"))
                .black()
                .bright()
        );
    }
    println!();
}

pub fn card(title: &str, fields: &[(&str, String)]) {
    let title = single_line(title);
    let fields = fields
        .iter()
        .map(|(label, value)| (single_line(label), single_line(value)))
        .collect::<Vec<_>>();
    let label_width = fields
        .iter()
        .map(|(label, _)| text_width(label))
        .max()
        .unwrap_or(0);
    let content_width = fields
        .iter()
        .map(|(_, value)| label_width + 2 + text_width(value))
        .max()
        .unwrap_or(0)
        .max(text_width(&title) + 2)
        .max(24);
    let total_width = content_width + 4;

    print_box_top(&title, total_width);
    for (label, value) in &fields {
        let label_padding = " ".repeat(label_width.saturating_sub(text_width(label)));
        let right_padding =
            " ".repeat(content_width.saturating_sub(label_width + 2 + text_width(value)));
        println!(
            "{} {}{}  {}{} {}",
            dialoguer::console::style("│").cyan(),
            dialoguer::console::style(label).black().bright(),
            label_padding,
            dialoguer::console::style(value).white(),
            right_padding,
            dialoguer::console::style("│").cyan()
        );
    }
    print_box_bottom(total_width);
}

pub fn code_block(title: &str, lines: &[&str]) {
    let title = single_line(title);
    let lines = lines
        .iter()
        .map(|line| single_line(line))
        .collect::<Vec<_>>();
    let content_width = lines
        .iter()
        .map(|line| text_width(line))
        .max()
        .unwrap_or(0)
        .max(text_width(&title) + 2)
        .max(24);
    let total_width = content_width + 4;

    print_box_top(&title, total_width);
    for line in &lines {
        let padding = " ".repeat(content_width.saturating_sub(text_width(line)));
        println!(
            "{} {}{} {}",
            dialoguer::console::style("│").cyan(),
            dialoguer::console::style(line).yellow(),
            padding,
            dialoguer::console::style("│").cyan()
        );
    }
    print_box_bottom(total_width);
}

pub fn steps(title: &str, items: &[&str]) {
    section(title, None);
    for item in items {
        println!(
            "  {} {}",
            dialoguer::console::style("→").cyan().bold(),
            single_line(item)
        );
    }
}

pub fn table(headers: &[&str], rows: &[Vec<String>]) {
    if headers.is_empty() {
        return;
    }
    let headers = headers
        .iter()
        .map(|header| single_line(header))
        .collect::<Vec<_>>();
    let rows = rows
        .iter()
        .map(|row| row.iter().map(|cell| single_line(cell)).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let widths = table_column_widths(&headers, &rows);

    println!("  {}", format_table_row(&headers, &widths, true));
    println!(
        "  {}",
        dialoguer::console::style(format_table_separator(&widths)).cyan()
    );
    for row in &rows {
        println!("  {}", format_table_row(row, &widths, false));
    }
}

fn table_column_widths(headers: &[String], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths = headers
        .iter()
        .map(|header| text_width(header))
        .collect::<Vec<_>>();
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(widths.len()) {
            widths[index] = widths[index].max(text_width(cell));
        }
    }
    widths
}

fn format_table_row(values: &[String], widths: &[usize], header: bool) -> String {
    let mut row = String::new();
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            row.push_str("  ");
        }
        let value = values.get(index).map_or("", String::as_str);
        let value = pad_to_width(value, *width);
        let styled = if header {
            dialoguer::console::style(value)
                .black()
                .bright()
                .to_string()
        } else {
            dialoguer::console::style(value).white().to_string()
        };
        row.push_str(&styled);
    }
    row
}

fn format_table_separator(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "─".repeat(*width))
        .collect::<Vec<_>>()
        .join("──")
}

fn pad_to_width(value: &str, width: usize) -> String {
    format!(
        "{value}{}",
        " ".repeat(width.saturating_sub(text_width(value)))
    )
}

fn print_box_top(title: &str, total_width: usize) {
    let title_width = text_width(title);
    let dash_count = total_width.saturating_sub(title_width + 5).max(1);
    let top = if title.is_empty() {
        format!("╭{}╮", "─".repeat(total_width.saturating_sub(2)))
    } else {
        format!("┌─ {title} {}┐", "─".repeat(dash_count))
    };
    println!("{}", dialoguer::console::style(top).cyan());
}

fn print_box_row(value: &str, content_width: usize, emphasize: bool) {
    let padding = " ".repeat(content_width.saturating_sub(text_width(value)));
    let value = if emphasize {
        dialoguer::console::style(value).cyan().bold()
    } else {
        dialoguer::console::style(value).white().bold()
    };
    println!(
        "{} {}{} {}",
        dialoguer::console::style("│").cyan(),
        value,
        padding,
        dialoguer::console::style("│").cyan()
    );
}

fn print_box_bottom(total_width: usize) {
    println!(
        "{}",
        dialoguer::console::style(format!("└{}┘", "─".repeat(total_width.saturating_sub(2))))
            .cyan()
    );
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            character => character,
        })
        .collect()
}

fn text_width(value: &str) -> usize {
    dialoguer::console::measure_text_width(value)
}

pub fn success(title: &str, detail: Option<&str>) {
    success_line(title);
    if let Some(detail) = detail {
        println!("  {detail}");
    }
}

pub fn success_line(message: &str) {
    println!();
    println!(
        "{} {}",
        dialoguer::console::style("✔").green().bold(),
        dialoguer::console::style(message).green().bold()
    );
}

pub fn success_value(label: &str, value: &str) {
    success_line(&format!("{label}: {value}"));
}

pub fn info(message: &str) {
    println!(
        "{} {}",
        dialoguer::console::style("·").cyan().bold(),
        message
    );
}

pub fn warning(message: &str) {
    eprintln!(
        "{} {}",
        dialoguer::console::style("!").yellow().bold(),
        message
    );
}

pub fn error(title: &str, detail: &str, code: &str, field: Option<&str>) {
    eprintln!();
    eprintln!(
        "{} {}",
        dialoguer::console::style("✖").red().bold(),
        dialoguer::console::style(title).red().bold()
    );
    eprintln!("  {detail}");
    eprintln!(
        "  {} {}",
        dialoguer::console::style("code").black().bright(),
        code
    );
    if let Some(field) = field {
        eprintln!(
            "  {} {}",
            dialoguer::console::style("field").black().bright(),
            field
        );
    }
    eprintln!();
}

fn print_fallback_prompt(prompt: &str, default: Option<&str>) -> Result<(), SdkError> {
    print!("{}", dialoguer::console::style("◆").cyan().bold());
    print!(" {}", prompt);
    if let Some(default) = default {
        print!(" [{}]", default);
    }
    print!(": ");
    io::stdout().flush().map_err(|_| prompt_error())
}

fn read_line() -> Result<String, SdkError> {
    let mut value = String::new();
    let bytes = io::stdin()
        .lock()
        .read_line(&mut value)
        .map_err(|_| prompt_error())?;
    if bytes == 0 {
        return Err(prompt_error());
    }
    while matches!(value.chars().last(), Some('\n' | '\r')) {
        let _ = value.pop();
    }
    Ok(value)
}
