use console::{style, Style, StyledObject};
use dialoguer::theme::Theme;
use std::fmt;

pub struct BasicTheme {
    pub defaults_style: Style,
    pub prompt_style: Style,
    pub prompt_prefix: StyledObject<String>,
    pub success_prefix: StyledObject<String>,
    pub error_prefix: StyledObject<String>,
    pub error_style: Style,
    pub hint_style: Style,
    pub values_style: Style,
    pub active_item_style: Style,
    pub inactive_item_style: Style,
    pub active_item_prefix: StyledObject<String>,
    pub inactive_item_prefix: StyledObject<String>,
}

impl Default for BasicTheme {
    fn default() -> BasicTheme {
        BasicTheme {
            defaults_style: Style::new().for_stderr().green().bold(),
            prompt_style: Style::new().for_stderr().bold(),
            prompt_prefix: style("?".to_string()).for_stderr().yellow(),
            success_prefix: style("+".to_string()).for_stderr().green(),
            error_prefix: style("-".to_string()).for_stderr().red(),
            error_style: Style::new().for_stderr().red().bold().italic(),
            hint_style: Style::new().for_stderr().blue().dim(),
            values_style: Style::new().for_stderr().green(),
            active_item_style: Style::new().for_stderr().cyan(),
            inactive_item_style: Style::new().for_stderr(),
            active_item_prefix: style(">".to_string()).for_stderr().green(),
            inactive_item_prefix: style(" ".to_string()).for_stderr(),
        }
    }
}

impl Theme for BasicTheme {
    /// Formats a prompt.
    fn format_prompt(&self, f: &mut dyn fmt::Write, prompt: &str) -> fmt::Result {
        if !prompt.is_empty() {
            write!(
                f,
                "{} {}",
                &self.prompt_prefix,
                self.prompt_style.apply_to(prompt)
            )?
        }
        Ok(())
    }

    /// Formats an error
    fn format_error(&self, f: &mut dyn fmt::Write, err: &str) -> fmt::Result {
        write!(
            f,
            "{} {}",
            &self.error_prefix,
            self.error_style.apply_to(err)
        )
    }

    /// Formats an input prompt.
    fn format_input_prompt(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        default: Option<&str>,
    ) -> fmt::Result {
        if !prompt.is_empty() {
            write!(
                f,
                "{} {}",
                &self.prompt_prefix,
                self.prompt_style.apply_to(prompt)
            )?;
        }

        match default {
            Some(default) => write!(
                f,
                " {} ",
                self.defaults_style.apply_to(&format!("({})", default)),
            ),
            None => write!(f, " "),
        }
    }

    /// Formats a confirm prompt.
    fn format_confirm_prompt(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        default: Option<bool>,
    ) -> fmt::Result {
        if !prompt.is_empty() {
            write!(
                f,
                "{} {} ",
                &self.prompt_prefix,
                self.prompt_style.apply_to(prompt)
            )?;
        }

        match default {
            None => write!(f, "{} ", self.hint_style.apply_to("(y/n)"),),
            Some(true) => write!(
                f,
                "({}{})",
                self.defaults_style.apply_to("y"),
                self.hint_style.apply_to("/n")
            ),
            Some(false) => write!(
                f,
                "({}{})",
                self.hint_style.apply_to("y/"),
                self.defaults_style.apply_to("n")
            ),
        }
    }

    /// Formats a confirm prompt after selection.
    fn format_confirm_prompt_selection(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        selection: Option<bool>,
    ) -> fmt::Result {
        if !prompt.is_empty() {
            write!(
                f,
                "{} {}",
                &self.success_prefix,
                self.prompt_style.apply_to(prompt)
            )?;
        }
        let selection = selection.map(|b| if b { "yes" } else { "no" });

        match selection {
            Some(selection) => write!(f, " {}", self.values_style.apply_to(selection)),
            None => Ok(()),
        }
    }

    /// Formats an input prompt after selection.
    fn format_input_prompt_selection(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        sel: &str,
    ) -> fmt::Result {
        if !prompt.is_empty() {
            write!(
                f,
                "{} {} ",
                &self.success_prefix,
                self.prompt_style.apply_to(prompt)
            )?;
        }

        write!(f, "{}", self.values_style.apply_to(sel))
    }

    /// Formats a select prompt item.
    fn format_select_prompt_item(
        &self,
        f: &mut dyn fmt::Write,
        text: &str,
        active: bool,
    ) -> fmt::Result {
        let details = if active {
            (
                &self.active_item_prefix,
                self.active_item_style.apply_to(text),
            )
        } else {
            (
                &self.inactive_item_prefix,
                self.inactive_item_style.apply_to(text),
            )
        };

        write!(f, "{} {}", details.0, details.1)
    }
}
