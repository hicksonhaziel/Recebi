#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' \
    'Usage: scripts/install-reconcile-cron.sh JOB_ID TELEGRAM_CHAT_ID' \
    '' \
    'Hardens an existing ZeroClaw Recebi agent cron job:' \
    '  - five-minute Africa/Lagos schedule;' \
    '  - memory disabled and one allowed Recebi tool;' \
    '  - Telegram announce delivery with fail-closed delivery errors;' \
    '  - bounded alert/quiet prompt; and' \
    '  - a mode-0600 SQLite backup before the update.' \
    '' \
    'Create the agent job first with `zeroclaw cron add --prompt`, then pass' \
    'its printed ID and the authorized Telegram peer/chat ID.' \
    '' \
    'Optional environment overrides:' \
    '  ZEROCLAW_CRON_DB       path to jobs.db' \
    '  ZEROCLAW_CRON_BACKUPS  backup directory' \
    '  RECEBI_CRON_EXPRESSION cron expression (default: */5 * * * *)' \
    '  RECEBI_CRON_RESTART    restart daemon after update (default: true)' \
    '  RECEBI_CRON_AGENT      configured agent alias (default: hickson)'
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
expression=${RECEBI_CRON_EXPRESSION:-'*/5 * * * *'}
agent_alias=${RECEBI_CRON_AGENT:-hickson}
restart=${RECEBI_CRON_RESTART:-true}
tool_name='recebi__recebi_reconcile_open'
prompt='Run exactly one Recebi reconciliation pass. Call only recebi__recebi_reconcile_open with {"max_count":10}. Do not retry and do not use raw HTTP or RPC tools. Return checked, payment_verified, pending, needs_review, and incomplete counts plus only bounded anomaly and incomplete IDs supplied by the tool. If payment_verified, needs_review, or incomplete is non-zero, send a compact operator alert. If all three are zero, return exactly NO_REPLY[INFO]: no new Recebi activity. Never infer payment from an error.'

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
if [[ ! $expression =~ ^[0-9*/?,[:space:]-]+$ ]]; then
  printf '%s\n' 'cron expression contains unsupported characters' >&2
  exit 2
fi
if [[ ! $restart == true && ! $restart == false ]]; then
  printf '%s\n' 'RECEBI_CRON_RESTART must be true or false' >&2
  exit 2
fi
if [[ $cron_db == *\'* ]]; then
  printf '%s\n' 'cron database path cannot contain a single quote' >&2
  exit 2
fi
if [[ $backup_dir == *\'* ]]; then
  printf '%s\n' 'cron backup directory cannot contain a single quote' >&2
  exit 2
fi

for command in zeroclaw sqlite3 sha256sum stat; do
  command -v "$command" >/dev/null || {
    printf 'missing required command: %s\n' "$command" >&2
    exit 2
  }
done
[[ -f $cron_db && -r $cron_db && -w $cron_db ]] || {
  printf 'cron database is not readable and writable: %s\n' "$cron_db" >&2
  exit 2
}

schema_ok=$(sqlite3 "$cron_db" \
  "SELECT COUNT(*) FROM pragma_table_info('cron_jobs') WHERE name IN ('id','expression','prompt','delivery','allowed_tools','uses_memory','agent_alias');")
[[ $schema_ok == 7 ]] || {
  printf '%s\n' 'cron database does not have the expected ZeroClaw schema' >&2
  exit 2
}

job_ok=$(sqlite3 "$cron_db" \
  "SELECT COUNT(*) FROM cron_jobs WHERE id='$job_id' AND job_type='agent' AND agent_alias='$agent_alias' AND allowed_tools='[\"$tool_name\"]';")
[[ $job_ok == 1 ]] || {
  printf '%s\n' \
    'the job must be one agent job owned by the requested alias and already' \
    'restricted to recebi__recebi_reconcile_open' >&2
  exit 3
}

mkdir -p -- "$backup_dir"
chmod 700 "$backup_dir"
backup_path="$backup_dir/jobs-before-recebi-$(date -u +%Y%m%dT%H%M%SZ).sqlite3"
sqlite3 "$cron_db" ".backup '$backup_path'"
chmod 600 "$backup_path"
[[ $(stat -c '%a' "$backup_path") == 600 ]] || {
  printf '%s\n' 'cron backup permissions are not 0600' >&2
  exit 4
}

# Let ZeroClaw calculate the next run using its own cron parser, then update
# delivery/prompt atomically. The CLI in 0.8.3 does not expose delivery flags.
zeroclaw cron update "$job_id" \
  --agent "$agent_alias" \
  --expression "$expression" \
  --tz Africa/Lagos \
  --allowed-tool "$tool_name" \
  --uses-memory false

delivery=$(printf '%s\n' "$telegram_chat_id" | awk '{printf "{\"mode\":\"announce\",\"channel\":\"telegram\",\"to\":\"%s\",\"best_effort\":false}", $0}')
updated_rows=$(sqlite3 "$cron_db" <<SQL
.parameter init
.parameter set :job_id '$job_id'
.parameter set :prompt '$prompt'
.parameter set :delivery '$delivery'
BEGIN IMMEDIATE;
UPDATE cron_jobs
SET prompt = :prompt,
    delivery = :delivery,
    uses_memory = 0
WHERE id = :job_id;
SELECT changes();
COMMIT;
SQL
)
[[ $updated_rows == 1 ]] || {
  printf '%s\n' 'cron job update affected no rows; inspect the backup before retrying' >&2
  exit 4
}

IFS=$'\t' read -r actual_expression actual_delivery actual_tools actual_memory actual_agent < <(
  sqlite3 -separator $'\t' "$cron_db" \
    "SELECT expression,delivery,allowed_tools,uses_memory,agent_alias FROM cron_jobs WHERE id='$job_id';"
)
[[ $actual_expression == "$expression" &&
   $actual_delivery == "$delivery" &&
   $actual_tools == "[\"$tool_name\"]" &&
   $actual_memory == 0 &&
   $actual_agent == "$agent_alias" ]] || {
  printf '%s\n' 'post-update cron verification failed; inspect the backup before retrying' >&2
  exit 4
}

if [[ $restart == true ]]; then
  zeroclaw service restart
fi

printf 'Recebi cron configured: %s\n' "$job_id"
printf '  schedule: %s (Africa/Lagos)\n' "$actual_expression"
printf '  delivery: Telegram peer %s (announce, fail closed)\n' "$telegram_chat_id"
printf '  backup:   %s (sha256 %s)\n' "$backup_path" "$(sha256sum "$backup_path" | awk '{print $1}')"
