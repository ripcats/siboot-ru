use std::collections::HashMap;

use serde::Deserialize;

pub const QUESTION: &str = "какой командой разработчиков подписать?";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Free,
    Paid,
    Unknown,
}

impl Kind {
    pub fn of(team_type: Option<&str>, memberships: &[String]) -> Kind {
        match (team_type, memberships) {
            (Some("Company/Organization"), _) => Kind::Paid,
            (Some("Individual"), [only]) if only.to_lowercase().contains("free") => Kind::Free,
            (Some("Individual"), [_, ..]) => Kind::Paid,
            _ => Kind::Unknown,
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Listed {
    pub team_id: String,
    pub r#type: Option<String>,
    #[serde(default)]
    pub memberships: Vec<Membership>,
}

#[derive(Deserialize, Debug)]
pub struct Membership {
    #[serde(default)]
    pub name: String,
}

pub fn kinds(listed: &[Listed]) -> HashMap<String, Kind> {
    listed
        .iter()
        .map(|team| {
            let names: Vec<String> = team.memberships.iter().map(|m| m.name.clone()).collect();
            (
                team.team_id.clone(),
                Kind::of(team.r#type.as_deref(), &names),
            )
        })
        .collect()
}

pub fn named(name: Option<&str>, id: &str) -> String {
    let name = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("unnamed");
    format!("{name} ({id})")
}

pub fn label(name: Option<&str>, id: &str, kind: Kind) -> String {
    match kind {
        Kind::Free => format!("Бесплатная · {}", named(name, id)),
        Kind::Paid => format!("Платная · {}", named(name, id)),
        Kind::Unknown => named(name, id),
    }
}

pub fn summary(name: Option<&str>, id: &str, kind: Kind) -> String {
    match kind {
        Kind::Free => format!("бесплатная команда {}", named(name, id)),
        Kind::Paid => format!("платная команда {}", named(name, id)),
        Kind::Unknown => format!("команда {}", named(name, id)),
    }
}

pub fn found(teams: usize) -> String {
    format!("Команд разработчиков в этой учётной записи: {teams}")
}

pub const NOTE: &str = "Приложения бесплатной команды действуют 7 дней, платной — год.";

pub fn lasts(kind: Kind, first: bool) -> &'static str {
    match (kind, first) {
        (Kind::Paid, _) => "Подписано платной командой: срок действия — год.",
        (_, true) => {
            "Приложения бесплатной учётной записи действуют 7 дней; затем SideInstaller сможет продлевать подпись самостоятельно."
        }
        (_, false) => {
            "Приложения бесплатной учётной записи действуют 7 дней. Для продления снова запустите siboot и выберите ту же команду."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn free_and_paid_as_altsign_tells_them() {
        let individual = Some("Individual");
        let company = Some("Company/Organization");
        assert_eq!(
            Kind::of(individual, &names(&["Some Free Program"])),
            Kind::Free
        );
        assert_eq!(Kind::of(individual, &names(&["free"])), Kind::Free);
        assert_eq!(
            Kind::of(individual, &names(&["Apple Developer Program"])),
            Kind::Paid
        );
        assert_eq!(
            Kind::of(individual, &names(&["Free", "Apple Developer Program"])),
            Kind::Paid
        );
        assert_eq!(
            Kind::of(company, &names(&["Apple Developer Program"])),
            Kind::Paid
        );
        assert_eq!(Kind::of(company, &[]), Kind::Paid);
        assert_eq!(Kind::of(individual, &[]), Kind::Unknown);
        assert_eq!(
            Kind::of(Some("Something Else"), &names(&["Free"])),
            Kind::Unknown
        );
        assert_eq!(Kind::of(None, &names(&["Free"])), Kind::Unknown);
    }

    #[test]
    fn kinds_come_from_a_list_teams_answer() {
        let answer = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-
<plist version="1.0">
<dict>
  <key>resultCode</key><integer>0</integer>
  <key>teams</key>
  <array>
    <dict>
      <key>name</key><string>Frizzle</string>
      <key>teamId</key><string>ABCDE12345</string>
      <key>type</key><string>Individual</string>
      <key>status</key><string>active</string>
      <key>dateCreated</key><date>2024-05-01T10:00:00Z</date>
      <key>memberships</key>
      <array>
        <dict>
          <key>name</key><string>Xcode Free Provisioning</string>
          <key>platform</key><string>ios</string>
          <key>dateStart</key><date>2024-05-01T10:00:00Z</date>
        </dict>
      </array>
    </dict>
    <dict>
      <key>name</key><string>Frizzle</string>
      <key>teamId</key><string>FGHIJ67890</string>
      <key>type</key><string>Individual</string>
      <key>memberships</key>
      <array>
        <dict><key>name</key><string>Apple Developer Program</string></dict>
      </array>
    </dict>
    <dict>
      <key>name</key><string>Example Ltd</string>
      <key>teamId</key><string>KLMNO24680</string>
      <key>type</key><string>Company/Organization</string>
    </dict>
    <dict>
      <key>teamId</key><string>PQRST13579</string>
      <key>type</key><string>Individual</string>
    </dict>
  </array>
</dict>
</plist>"#;
        let dict: plist::Dictionary = plist::from_bytes(answer).unwrap();
        let listed: Vec<Listed> = plist::from_value(dict.get("teams").unwrap()).unwrap();
        let kinds = kinds(&listed);
        assert_eq!(kinds["ABCDE12345"], Kind::Free);
        assert_eq!(kinds["FGHIJ67890"], Kind::Paid);
        assert_eq!(kinds["KLMNO24680"], Kind::Paid);
        assert_eq!(kinds["PQRST13579"], Kind::Unknown);
    }

    #[test]
    fn labels_lead_with_the_kind() {
        assert_eq!(
            label(Some("Frizzle"), "ABCDE12345", Kind::Free),
            "Free · Frizzle (ABCDE12345)"
        );
        assert_eq!(
            label(Some("Frizzle"), "FGHIJ67890", Kind::Paid),
            "Paid · Frizzle (FGHIJ67890)"
        );
        assert_eq!(
            label(Some("Frizzle"), "ABCDE12345", Kind::Unknown),
            "Frizzle (ABCDE12345)"
        );
        assert_eq!(
            label(None, "PQRST13579", Kind::Unknown),
            "unnamed (PQRST13579)"
        );
        assert_eq!(
            label(Some("  "), "PQRST13579", Kind::Free),
            "Free · unnamed (PQRST13579)"
        );
        assert_eq!(
            summary(Some("Frizzle"), "FGHIJ67890", Kind::Paid),
            "paid team Frizzle (FGHIJ67890)"
        );
        assert_eq!(
            summary(Some("Frizzle"), "ABCDE12345", Kind::Unknown),
            "team Frizzle (ABCDE12345)"
        );
    }

    #[test]
    fn the_step_line_fits_an_upright_iphone() {
        assert_eq!(found(2), "2 teams on this Apple Account");
        assert!(found(9).chars().count() <= 31);
    }

    #[test]
    fn the_finish_box_says_how_long_it_lasts() {
        assert_eq!(lasts(Kind::Paid, false), lasts(Kind::Paid, true));
        assert!(lasts(Kind::Paid, true).contains("a year"));
        assert!(lasts(Kind::Free, true).contains("renew itself"));
        assert!(lasts(Kind::Unknown, true).contains("renew itself"));
        assert!(lasts(Kind::Free, false).contains("run siboot again"));
        assert!(!lasts(Kind::Free, false).contains("renew itself"));
    }
}
