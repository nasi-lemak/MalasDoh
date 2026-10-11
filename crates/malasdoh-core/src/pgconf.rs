//! PostgreSQL configuration: the save check for `postgresql.conf`,
//! `pg_hba.conf` and friends, and warnings for settings a version no longer
//! accepts (what usually stops a freshly upgraded cluster from starting).
//!
//! The check runs on the server after the file is written, and the original
//! goes back if it fails:
//! - every file: the cluster's own `postgres -C`, which reads the whole
//!   configuration (includes too) and refuses unknown settings, bad values
//!   and syntax errors, whether the server is running or not;
//! - `pg_hba.conf` / `pg_ident.conf`: the running server's
//!   `pg_hba_file_rules` / `pg_ident_file_mappings`, which re-read the file
//!   and report bad lines. A stopped server cannot check them, so the save
//!   is then reported as not checked.
//!
//! On success it says which changes still need a restart (a reload is not
//! enough for settings like `shared_buffers`).

use crate::ssh::shell_quote;

/// Rule commands that stand for the scripts below.
pub const CHECK: &str = "builtin:postgresql";
pub const RELOAD: &str = "builtin:postgresql-reload";

/// Find the cluster that `$F` belongs to and how to run its programs.
/// Exits 127 (shown as "not checked", the save stands) when it cannot.
const PRELUDE: &str = r#"
cd / || exit 127
skip() { echo "$*"; exit 127; }
F=$(readlink -f "$F" 2>/dev/null || echo "$F")
conf= data= ver= bin=
if command -v pg_lsclusters >/dev/null 2>&1; then
  # Debian/Ubuntu: configuration in /etc/postgresql/<version>/<name>.
  while read -r v name port status owner dir rest; do
    c=/etc/postgresql/$v/$name
    case "$F" in "$c"/*|"$dir"/*) conf=$c; data=$dir; ver=$v; break;; esac
  done <<PGLS
$(pg_lsclusters --no-header 2>/dev/null)
PGLS
fi
if [ -z "$conf" ]; then
  # Elsewhere the configuration lives in the data folder (it has PG_VERSION).
  d=$(dirname "$F")
  while [ "$d" != / ] && [ ! -f "$d/PG_VERSION" ]; do d=$(dirname "$d"); done
  [ -f "$d/PG_VERSION" ] || skip "$F is not in a PostgreSQL configuration or data folder"
  conf=$d; data=$d; ver=$(cat "$d/PG_VERSION")
fi
for b in /usr/lib/postgresql/$ver/bin /usr/pgsql-$ver/bin /usr/local/pgsql/bin /usr/bin "$(pg_config --bindir 2>/dev/null)"; do
  [ -x "$b/postgres" ] || continue
  case "$("$b/postgres" -V 2>/dev/null)" in *" $ver."*|*" $ver") bin=$b; break;; esac
done
[ -n "$bin" ] || skip "the PostgreSQL $ver server program was not found"
owner=$(stat -c %U "$data" 2>/dev/null || stat -c %U "$conf")
if [ "$(id -un)" = "$owner" ]; then run() { "$@"; }
elif [ "$(id -u)" = 0 ] && command -v runuser >/dev/null 2>&1; then run() { runuser -u "$owner" -- "$@"; }
elif [ "$(id -u)" = 0 ] || sudo -n -u "$owner" true 2>/dev/null; then run() { sudo -n -u "$owner" -- "$@"; }
else skip "the check has to run as $owner; edit the file as root (sudo) to have it checked"
fi
setting() { run "$bin/postgres" -D "$conf" -C "$1" 2>/dev/null; }
# Ask the running server, if it is this cluster's and it answers.
up=
port=$(setting port)
sock=$(setting unix_socket_directories | cut -d, -f1 | sed 's/^ *//')
[ -n "$sock" ] || sock=/tmp
q() { run "$bin/psql" -X -A -t -q -h "$sock" -p "$port" -d postgres -c "$1" 2>&1; }
same() { [ "$(readlink -f "$1" 2>/dev/null || echo "$1")" = "$2" ]; }
if cf=$(q "show config_file") && same "$cf" "$(readlink -f "$conf/postgresql.conf" 2>/dev/null || echo "$conf/postgresql.conf")"; then up=1; fi
restart_needed() {
  q "select string_agg(name, ', ' order by name) from (select distinct on (f.name) f.name, f.applied, s.context
     from pg_file_settings f join pg_settings s using (name) order by f.name, f.seqno desc) x
     where not applied and context = 'postmaster'"
}
"#;

const CHECK_BODY: &str = r#"
# The whole configuration, as the server would read it at start.
if ! out=$(run "$bin/postgres" -D "$conf" -C data_directory 2>&1); then
  printf '%s\n' "$out" | sed -E 's/^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9:.]+ [^ ]+ \[[0-9]+\] //'
  exit 1
fi
case "$(basename "$F")" in
  pg_hba.conf|pg_ident.conf)
    [ -n "$up" ] || skip "PostgreSQL $ver is not running here, and only a running server can check $(basename "$F"); it is checked when the server starts"
    if [ "$(basename "$F")" = pg_hba.conf ]; then
      same "$(q 'show hba_file')" "$F" || skip "the running server reads another pg_hba.conf"
      bad=$(q "select string_agg('line ' || line_number || ': ' || error, E'\n' order by line_number) from pg_hba_file_rules where error is not null")
    else
      same "$(q 'show ident_file')" "$F" || skip "the running server reads another pg_ident.conf"
      bad=$(q "select string_agg('line ' || line_number || ': ' || error, E'\n' order by line_number) from pg_ident_file_mappings where error is not null") \
        || skip "PostgreSQL $ver cannot check pg_ident.conf (15 and newer can)"
    fi
    [ -z "$bad" ] || { printf '%s\n' "$bad"; exit 1; }
    echo "Reload PostgreSQL $ver to apply it."
    ;;
  *)
    if [ -n "$up" ]; then
      r=$(restart_needed)
      if [ -n "$r" ]; then echo "Needs a restart of PostgreSQL $ver to take effect: $r. The rest applies on reload."
      else echo "Reload PostgreSQL $ver to apply it."; fi
    else
      echo "PostgreSQL $ver is not running; the configuration is valid for its next start."
    fi
    ;;
esac
"#;

const RELOAD_BODY: &str = r#"
[ -n "$up" ] || { echo "PostgreSQL $ver is not running; nothing to reload."; exit 0; }
q "select pg_reload_conf()" >/dev/null || { echo "could not reload PostgreSQL $ver"; exit 1; }
sleep 1
r=$(q "select string_agg(name, ', ' order by name) from pg_settings where pending_restart")
if [ -n "$r" ]; then echo "Still needs a restart to take effect: $r."; fi
"#;

fn script(path: &str, body: &str) -> String {
    format!("F={}\n{PRELUDE}{body}", shell_quote(path))
}

/// The save check for `path`.
pub fn check_script(path: &str) -> String {
    script(path, CHECK_BODY)
}

/// Reload the cluster `path` belongs to and say what still needs a restart.
pub fn reload_script(path: &str) -> String {
    script(path, RELOAD_BODY)
}

/// The PostgreSQL major version a config path belongs to, when the path says
/// (`/etc/postgresql/17/main/…`, `/var/lib/pgsql/17/data/…`).
pub fn version_from_path(path: &str) -> Option<u32> {
    let parts: Vec<&str> = path.split('/').collect();
    parts.windows(2).find_map(|w| match w[0] {
        "postgresql" | "pgsql" => major(w[1]),
        _ => None,
    })
}

fn major(s: &str) -> Option<u32> {
    let n: u32 = s.split('.').next()?.parse().ok()?;
    // "9.6" is major 9 (with minor 6); 10 and later are plain numbers.
    (9..100).contains(&n).then_some(n)
}

/// Is this a file PostgreSQL reads its settings from?
pub fn is_settings_file(path: &str) -> bool {
    let name = crate::sftp::file_name(path);
    name == "postgresql.conf"
        || name == "postgresql.auto.conf"
        || (path.starts_with("/etc/postgresql/") && name.ends_with(".conf") && !name.starts_with("pg_"))
}

/// Settings a release dropped: the version, and what to use instead.
const REMOVED: &[(&str, u32, &str)] = &[
    ("checkpoint_segments", 9, "max_wal_size (and min_wal_size), in size units, not segments"),
    ("min_parallel_relation_size", 10, "min_parallel_table_scan_size"),
    ("sql_inheritance", 10, "nothing: inheritance is always on; write ONLY in queries to skip children"),
    ("replacement_sort_tuples", 11, "nothing: it was dropped"),
    ("standby_mode", 12, "a standby.signal file in the data folder"),
    ("trigger_file", 12, "promote_trigger_file up to 15, then pg_ctl promote or pg_promote()"),
    ("wal_keep_segments", 13, "wal_keep_size, in size units: segments × 16MB"),
    ("operator_precedence_warning", 14, "nothing: it was dropped"),
    ("vacuum_cleanup_index_scale_factor", 14, "nothing: it was dropped"),
    ("stats_temp_directory", 15, "nothing: statistics now live in shared memory"),
    ("promote_trigger_file", 16, "pg_ctl promote or SELECT pg_promote()"),
    ("vacuum_defer_cleanup_age", 16, "hot_standby_feedback or a replication slot"),
    ("force_parallel_mode", 16, "debug_parallel_query"),
    ("old_snapshot_threshold", 17, "nothing: it was dropped"),
    ("db_user_namespace", 17, "nothing: it was dropped"),
    ("trace_recovery_messages", 17, "nothing: it was dropped"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// 1-based.
    pub line: usize,
    pub message: String,
}

/// Settings in `text` that the PostgreSQL version of `path` (or, when the
/// path does not say, any current version) no longer accepts. Only for
/// PostgreSQL settings files; anything else gives nothing.
pub fn lint(path: &str, text: &str) -> Vec<Finding> {
    if !is_settings_file(path) {
        return Vec::new();
    }
    let version = version_from_path(path);
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let l = line.trim_start();
        if l.starts_with('#') {
            continue;
        }
        let name: String = l.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.').collect();
        // `name = value` or `name value`.
        let after = &l[name.len()..];
        if name.is_empty() || !(after.is_empty() || after.starts_with(|c: char| c == '=' || c.is_whitespace())) {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        let Some(&(_, since, instead)) = REMOVED.iter().find(|(n, _, _)| *n == lower) else { continue };
        let msg = match version {
            Some(v) if v < since => continue,
            Some(v) => format!(
                "{name} is not accepted by PostgreSQL {v} (removed in {since}); PostgreSQL will refuse to start. Use {instead}."
            ),
            None => format!("{name} was removed in PostgreSQL {since}. Use {instead}."),
        };
        out.push(Finding { line: i + 1, message: msg });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_from_paths() {
        assert_eq!(version_from_path("/etc/postgresql/17/main/postgresql.conf"), Some(17));
        assert_eq!(version_from_path("/var/lib/pgsql/16/data/pg_hba.conf"), Some(16));
        assert_eq!(version_from_path("/etc/postgresql/9.6/main/postgresql.conf"), Some(9));
        assert_eq!(version_from_path("/var/lib/postgresql/data/postgresql.conf"), None);
        assert_eq!(version_from_path("/srv/app/x.conf"), None);
    }

    #[test]
    fn flags_removed_settings_for_the_target_version() {
        let text = "# wal_keep_segments = 1\nwal_keep_segments = 64\nshared_buffers = 1GB\n  force_parallel_mode=on\n";
        let f = lint("/etc/postgresql/17/main/postgresql.conf", text);
        assert_eq!(f.iter().map(|f| f.line).collect::<Vec<_>>(), vec![2, 4]);
        assert!(f[0].message.contains("wal_keep_size"));
        assert!(f[1].message.contains("debug_parallel_query"));
        // Still fine in 15: only the setting removed by then is flagged.
        let f = lint("/etc/postgresql/15/main/postgresql.conf", text);
        assert_eq!(f.iter().map(|f| f.line).collect::<Vec<_>>(), vec![2]);
        // Unknown version: everything known, without claiming a refusal.
        assert_eq!(lint("/opt/db/postgresql.conf", text).len(), 2);
        // Not a PostgreSQL file.
        assert!(lint("/etc/nginx/nginx.conf", text).is_empty());
        assert!(lint("/etc/postgresql/17/main/pg_hba.conf", text).is_empty());
    }

    #[test]
    fn scripts_quote_the_path() {
        assert!(
            check_script("/etc/postgresql/17/main/it's.conf").starts_with(r"F='/etc/postgresql/17/main/it'\''s.conf'")
        );
        assert!(reload_script("/x").contains("pg_reload_conf"));
    }
}
