use crate::utils::handle_error;
use dialoguer::Password;

pub(crate) fn get_password() -> Option<String> {
    let password = Password::new()
        .allow_empty_password(true)
        .with_prompt("Enter your repository password (leave empty to skip encryption)")
        .interact()
        .unwrap();

    let password = if !password.is_empty() {
        let confirm = Password::new()
            .with_prompt("Repeat password")
            .allow_empty_password(false)
            .interact()
            .unwrap();

        if password != confirm {
            handle_error("Error: the passwords don't match.".to_string(), None);
        }

        Some(password)
    } else {
        None
    };

    password
}
