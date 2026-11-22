use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

pub const FILE: &str = "apple-accounts";

pub const MAX: usize = 7;

pub const QUESTION: &str = "с какой учётной записью Apple войти?";
pub const ANOTHER: &str = "Другая учётная запись Apple";
pub const FORGET: &str = "Удалить учётную запись Apple";
pub const FORGET_WHICH: &str = "какую учётную запись Apple удалить?";

pub const SAVED: &str = "В следующий раз siboot предложит эту учётную запись и не запросит пароль, пока действует вход Apple. Сохраняется адрес почты, но не пароль.";
pub const RUN_OUT: &str = "Сохранённый вход истёк — Apple снова требует пароль.";

pub fn path(home: &Path) -> PathBuf {
    home.join(FILE)
}

pub fn exists(home: &Path) -> bool {
    path(home).is_file()
}

pub fn list(home: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path(home)).unwrap_or_default();
    tidy(text.lines())
}

fn tidy<'a>(emails: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for email in emails {
        let email = email.trim().to_lowercase();
        let plausible = email.contains('@') && !email.chars().any(char::is_whitespace);
        if plausible && !out.contains(&email) {
            out.push(email);
        }
    }
    out.truncate(MAX);
    out
}

pub fn remember(home: &Path, email: &str) -> io::Result<bool> {
    let email = email.trim().to_lowercase();
    let before = list(home);
    let new = !before.contains(&email);
    let mut accounts = vec![email.as_str()];
    accounts.extend(before.iter().map(String::as_str));
    write(home, &tidy(accounts))?;
    Ok(new)
}

pub fn forget(home: &Path, email: &str) -> io::Result<bool> {
    let email = email.trim().to_lowercase();
    let mut accounts = list(home);
    let had = accounts.len();
    accounts.retain(|a| *a != email);
    if accounts.len() == had {
        return Ok(false);
    }
    write(home, &accounts)?;
    Ok(true)
}

pub fn start(home: &Path, emails: &[String]) -> io::Result<()> {
    write(home, &tidy(emails.iter().map(String::as_str)))
}

fn write(home: &Path, accounts: &[String]) -> io::Result<()> {
    std::fs::create_dir_all(home)?;
    let path = path(home);
    let tmp = path.with_extension("tmp");
    let mut text = accounts.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let mut file = options.open(&tmp)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(text.as_bytes())?;
    }
    #[cfg(not(unix))]
    options.open(&tmp)?.write_all(text.as_bytes())?;
    std::fs::rename(tmp, path)
}

pub fn options(saved: &[String]) -> Vec<String> {
    let mut options = saved.to_vec();
    options.push(ANOTHER.to_string());
    options.push(FORGET.to_string());
    options
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Account(usize),
    Another,
    Forget,
}

pub fn picked(index: usize, saved: usize) -> Pick {
    match index.checked_sub(saved) {
        None => Pick::Account(index),
        Some(0) => Pick::Another,
        Some(_) => Pick::Forget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("siboot-saved-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        home
    }

    #[test]
    fn nothing_saved_is_an_empty_list() {
        let home = home("none");
        assert!(!exists(&home));
        assert!(list(&home).is_empty());
    }

    #[test]
    fn the_one_used_last_comes_first_once() {
        let home = home("order");
        assert!(remember(&home, "first@example.com").unwrap());
        assert!(remember(&home, "second@example.com").unwrap());
        assert_eq!(list(&home), ["second@example.com", "first@example.com"]);
        assert!(!remember(&home, "  First@Example.com ").unwrap());
        assert_eq!(list(&home), ["first@example.com", "second@example.com"]);
        assert_eq!(
            std::fs::read_to_string(path(&home)).unwrap(),
            "first@example.com\nsecond@example.com\n"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn the_oldest_goes_past_the_most_kept() {
        let home = home("max");
        for n in 0..MAX + 2 {
            remember(&home, &format!("n{n}@example.com")).unwrap();
        }
        let kept = list(&home);
        assert_eq!(kept.len(), MAX);
        assert_eq!(kept[0], format!("n{}@example.com", MAX + 1));
        assert!(!kept.contains(&"n0@example.com".to_string()));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn forgetting_takes_one_off_and_keeps_the_list() {
        let home = home("forget");
        remember(&home, "a@example.com").unwrap();
        remember(&home, "b@example.com").unwrap();
        assert!(forget(&home, "A@example.com").unwrap());
        assert_eq!(list(&home), ["b@example.com"]);
        assert!(!forget(&home, "a@example.com").unwrap());
        assert!(forget(&home, "b@example.com").unwrap());
        assert!(list(&home).is_empty());
        assert!(exists(&home));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_hand_edited_file_is_read_as_far_as_it_makes_sense() {
        let home = home("edited");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            path(&home),
            "Jane@iCloud.com\n\n  not an email\njane@icloud.com\nwith space@x.com\nbob@example.org",
        )
        .unwrap();
        assert_eq!(list(&home), ["jane@icloud.com", "bob@example.org"]);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn starting_the_list_writes_it_even_empty() {
        let home = home("start");
        start(&home, &[]).unwrap();
        assert!(exists(&home));
        assert!(list(&home).is_empty());
        start(&home, &["new@example.com".into(), "old@example.com".into()]).unwrap();
        assert_eq!(list(&home), ["new@example.com", "old@example.com"]);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn only_this_user_can_read_it() {
        use std::os::unix::fs::PermissionsExt;
        let home = home("mode");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(path(&home).with_extension("tmp"), "").unwrap();
        std::fs::set_permissions(
            path(&home).with_extension("tmp"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        remember(&home, "a@example.com").unwrap();
        let mode = std::fs::metadata(path(&home)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn the_menu_lists_the_accounts_then_the_other_two() {
        let saved = vec!["a@example.com".to_string(), "b@example.com".to_string()];
        assert_eq!(
            options(&saved),
            ["a@example.com", "b@example.com", ANOTHER, FORGET]
        );
        assert_eq!(picked(0, 2), Pick::Account(0));
        assert_eq!(picked(1, 2), Pick::Account(1));
        assert_eq!(picked(2, 2), Pick::Another);
        assert_eq!(picked(3, 2), Pick::Forget);
        assert!(options(&vec!["x@y.z".to_string(); MAX]).len() <= 9);
    }
}
