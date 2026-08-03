#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' \
    'Usage: scripts/install-hot-reconcile-cron.sh JOB_ID TELEGRAM_CHAT_ID' \
    '' \
    'Configures the lightweight Recebi hot watchdog.' \
    'No recent invoice: the worker exits immediately.' \
    'Recent invoice: it checks every five seconds for at most three minutes.' \
    'Recebi checks only invoices created in the last three minutes.' \
    '' \
    'Create the shell job first with:' \
    '  zeroclaw cron add-every 1000 "ABSOLUTE_REPO/scripts/run-reconcile-job.sh hot CHAT_ID" --agent AGENT' \
    '' \
    'Optional environment overrides:' \
    '  ZEROCLAW_CRON_DB       path to jobs.db' \
    '  ZEROCLAW_CRON_BACKUPS  backup directory' \
    '  RECEBI_CRON_AGENT      configured agent alias (default: hickson)' \
    '  RECEBI_CRON_RESTART    restart daemon after update (default: true)'
}

if [[ ${1-} == '--help' || ${1-} == '-h' ]]; then
  usage
  exit 0
fi
if [[ $# -ne 2 ]]; then
  usage >&2
  exit 2
fi

job_id=$1
telegram_chat_id=$2
cron_db=${ZEROCLAW_CRON_DB:-"$HOME/.zeroclaw/data/cron/jobs.db"}
backup_dir=${ZEROCLAW_CRON_BACKUPS:-"$(dirname -- "$cron_db")/backups"}
agent_alias=${RECEBI_CRON_AGENT:-hickson}
restart=${RECEBI_CRON_RESTART:-true}
repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
expected_command="$repo_dir/scripts/run-reconcile-job.sh hot $telegram_chat_id"

if [[ ! $job_id =~ ^[A-Za-z0-9-]{8,128}$ ]]; then
  printf '%s\n' 'invalid cron job ID' >&2
  exit 2
fi
if [[ ! $telegram_chat_id =~ ^-?[0-9]+$ ]]; then
  printf '%s\n' 'Telegram chat ID must be a numeric peer ID' >&2
  exit 2
fi
if [[ ! $agent_alias =~ ^[A-Za-z0-9_-]{1,64}$ ]]; then
  printf '%s\n' 'invalid ZeroClaw agent alias' >&2
  exit 2
fi
if [[ $restart != true && $restart != false ]]; then
  printf '%s\n' 'RECEBI_CRON_RESTART must be true or false' >&2
  exit 2
fi
if [[ $cron_db == *\'* || $backup_dir == *\'* || $expected_command == *\'* ]]; then
  printf '%s\n' 'cron paths and command cannot contain a single quote' >&2
  exit 2
fi

for command in zeroclaw sqlite3 sha256sum stat; do
  command -v "$command" >/dev/null || {
    printf 'missing required command: %s\n' "$command" >&2
    exit 2
  }
done
[[ -x $repo_dir/scripts/run-reconcile-job.sh ]] || {
  printf '%s\n' 'hot reconciliation runner is not executable' >&2
  exit 2
}
[[ -f $cron_db && -r $cron_db && -w $cron_db ]] || {
  printf 'cron database is not readable and writable: %s\n' "$cron_db" >&2
  exit 2
}

schema_ok=$(sqlite3 "$cron_db" \
  "SELECT COUNT(*) FROM pragma_table_info('cron_jobs') WHERE name IN ('id','command','delivery','allowed_tools','uses_memory','agent_alias','enabled','schedule');")
[[ $schema_ok == 8 ]] || {
  printf '%s\n' 'cron database does not have the expected ZeroClaw schema' >&2
  exit 2
}

job_ok=$(sqlite3 "$cron_db" \
  "SELECT COUNT(*) FROM cron_jobs WHERE id='$job_id' AND job_type='shell' AND agent_alias='$agent_alias' AND command='$expected_command' AND json_extract(schedule,'$.kind')='every' AND json_extract(schedule,'$.every_ms') IN (1000,5000,60000);")
[[ $job_ok == 1 ]] || {
  printf '%s\n' 'the job must be the exact Recebi hot shell command at an accepted migration interval' >&2
  exit 3
}

mkdir -p -- "$backup_dir"
chmod 700 "$backup_dir"
backup_path="$backup_dir/jobs-before-recebi-hot-$(date -u +%Y%m%dT%H%M%SZ).sqlite3"
sqlite3 "$cron_db" ".backup '$backup_path'"
chmod 600 "$backup_path"
[[ $(stat -c '%a' "$backup_path") == 600 ]] || {
  printf '%s\n' 'cron backup permissions are not 0600' >&2
  exit 4
}

updated_rows=$(sqlite3 "$cron_db" <<SQL
.parameter init
.parameter set :job_id '$job_id'
BEGIN IMMEDIATE;
UPDATE cron_jobs
SET expression = '',
    schedule = '{"kind":"every","every_ms":1000}',
    next_run = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+1 second'),
    delivery = NULL,
    allowed_tools = NULL,
    uses_memory = 0,
    enabled = 1
WHERE id = :job_id;
SELECT changes();
COMMIT;
SQL
)
[[ $updated_rows == 1 ]] || {
  printf '%s\n' 'hot cron update affected no rows; inspect the backup before retrying' >&2
  exit 4
}

IFS='|' read -r actual_schedule actual_command actual_delivery actual_tools actual_memory actual_agent actual_enabled < <(
  sqlite3 -separator '|' "$cron_db" \
    "SELECT schedule,command,COALESCE(delivery,''),COALESCE(allowed_tools,''),uses_memory,agent_alias,enabled FROM cron_jobs WHERE id='$job_id';"
)
[[ $actual_schedule == *'"kind":"every"'* &&
   $actual_schedule == *'"every_ms":1000'* &&
   $actual_command == "$expected_command" &&
   -z $actual_delivery &&
   -z $actual_tools &&
   $actual_memory == 0 &&
   $actual_agent == "$agent_alias" &&
   $actual_enabled == 1 ]] || {
  printf '%s\n' 'post-update hot cron verification failed; inspect the backup before retrying' >&2
  exit 4
}

if [[ $restart == true ]]; then
  zeroclaw service restart
fi

printf 'Recebi hot cron configured: %s\n' "$job_id"
printf '  schedule: lightweight watchdog every 1 second\n'
printf '  cadence:  active invoices every 5 seconds for at most 3 minutes\n'
printf '  delivery: deterministic Telegram send to peer %s\n' "$telegram_chat_id"
printf '  enabled:  yes\n'
printf '  backup:   %s (sha256 %s)\n' "$backup_path" "$(sha256sum "$backup_path" | awk '{print $1}')"
