variable "project_id" {
  description = "Google Cloud project for the deployment."
  type        = string
}

variable "region" {
  description = "Google Cloud region for regional resources."
  type        = string
  default     = "us-central1"
}

variable "zone" {
  description = "Google Cloud zone for the VM."
  type        = string
  default     = "us-central1-a"
}

variable "name" {
  description = "Lowercase name used to prefix Google Cloud resources."
  type        = string
  default     = "solana-microscope"

  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{2,31}$", var.name))
    error_message = "name must be 3-32 lowercase letters, numbers, or hyphens and start with a letter."
  }
}

variable "program_id" {
  description = "Solana program monitored by this deployment."
  type        = string

  validation {
    condition     = can(regex("^[1-9A-HJ-NP-Za-km-z]{32,44}$", var.program_id))
    error_message = "program_id must be a base58 Solana public key."
  }
}

variable "multisig" {
  description = "Optional Squads default vault, internal state account, and generation. Null disables multisig monitoring. Records label the configured vault vault_address and the state account multisig_address."
  type = object({
    vault_address = string
    state_address = string
    version       = string
  })
  default = null

  validation {
    condition = var.multisig == null ? true : (
      can(regex("^[1-9A-HJ-NP-Za-km-z]{32,44}$", var.multisig.vault_address)) &&
      can(regex("^[1-9A-HJ-NP-Za-km-z]{32,44}$", var.multisig.state_address)) &&
      contains(["v3", "v4", "v5"], var.multisig.version)
    )
    error_message = "multisig vault_address and state_address must be base58 Solana public keys, and version must be v3, v4, or v5."
  }
}

variable "idl_path" {
  description = "IDL path relative to this Terraform directory."
  type        = string
  default     = "../../idl/idl1.json"
}

variable "datasource" {
  description = "Live transaction datasource. Yellowstone is the backward-compatible default; RPC polls confirmed signatures."
  type = object({
    mode                  = optional(string, "yellowstone")
    poll_interval_seconds = optional(number, 5)
    replay_window_slots   = optional(number, 300)
  })
  default = {}

  validation {
    condition     = contains(["yellowstone", "rpc"], var.datasource.mode)
    error_message = "datasource.mode must be yellowstone or rpc."
  }

  validation {
    condition     = var.datasource.poll_interval_seconds >= 1 && var.datasource.poll_interval_seconds <= 300
    error_message = "datasource.poll_interval_seconds must be between 1 and 300."
  }

  validation {
    condition     = var.datasource.replay_window_slots >= 1 && var.datasource.replay_window_slots <= 100000
    error_message = "datasource.replay_window_slots must be between 1 and 100000."
  }
}

variable "geyser_url" {
  description = "Yellowstone gRPC endpoint. Required in yellowstone mode."
  type        = string
  sensitive   = true
  default     = ""

  validation {
    condition = (
      var.geyser_url == "" ||
      startswith(var.geyser_url, "https://") ||
      startswith(var.geyser_url, "http://")
    )
    error_message = "geyser_url must be empty or an HTTP(S) URL."
  }
}

variable "geyser_x_token" {
  description = "Optional Yellowstone gRPC authentication token."
  type        = string
  sensitive   = true
  default     = ""
}

variable "rpc_url" {
  description = "Solana JSON-RPC endpoint. Required in RPC mode; enables Yellowstone gap recovery when set and is used by backfill."
  type        = string
  sensitive   = true
  default     = ""

  validation {
    condition = (
      var.rpc_url == "" ||
      startswith(var.rpc_url, "https://") ||
      startswith(var.rpc_url, "http://")
    )
    error_message = "rpc_url must be empty or an HTTP(S) URL."
  }
}

variable "grafana_admin_password" {
  description = "Initial Grafana administrator password. Required unless grafana_cloud is set."
  type        = string
  sensitive   = true
  default     = ""

  validation {
    condition     = var.grafana_admin_password == "" || length(var.grafana_admin_password) >= 12
    error_message = "grafana_admin_password must contain at least 12 characters."
  }
}

variable "grafana_cloud" {
  description = "Ship to a hosted Grafana Cloud stack (docker-compose.cloud.yml) instead of running Grafana, Loki, and Prometheus on the VM. Alert delivery is then handled by the hosted stack's contact points, not by channel secrets. deployment_name only namespaces data, it is not an isolation boundary: give each deployment its own revocable access-policy token and keep different trust domains in separate stacks."
  type = object({
    loki_url        = string
    loki_user       = string
    prom_url        = string
    prom_user       = string
    token           = string
    deployment_name = string
    environment     = optional(string, "prd")
  })
  sensitive = true
  default   = null

  validation {
    condition = (
      var.grafana_cloud == null ||
      can(regex("^[a-z0-9-]+$", var.grafana_cloud.deployment_name))
    )
    error_message = "grafana_cloud.deployment_name must be lowercase alphanumeric/hyphens."
  }

  validation {
    condition = (
      var.grafana_cloud == null || (
        length(trimspace(var.grafana_cloud.loki_url)) > 0 &&
        length(trimspace(var.grafana_cloud.loki_user)) > 0 &&
        length(trimspace(var.grafana_cloud.prom_url)) > 0 &&
        length(trimspace(var.grafana_cloud.prom_user)) > 0 &&
        length(trimspace(var.grafana_cloud.token)) > 0
      )
    )
    error_message = "grafana_cloud requires loki_url, loki_user, prom_url, prom_user, and token."
  }
}

variable "slack_webhook_url" {
  description = "Slack incoming webhook URL. Required when an alert uses the slack channel."
  type        = string
  sensitive   = true
  default     = ""
}

variable "telegram_bot_token" {
  description = "Telegram bot token. Required when an alert uses the telegram channel."
  type        = string
  sensitive   = true
  default     = ""
}

variable "telegram_chat_id" {
  description = "Telegram destination chat ID. Required when an alert uses the telegram channel."
  type        = string
  sensitive   = true
  default     = ""
}

variable "pagerduty_integration_key" {
  description = "PagerDuty Events API integration key. Required when an alert uses the pagerduty channel."
  type        = string
  sensitive   = true
  default     = ""
}

variable "dashboard" {
  description = "Optional JSON field paths overriding the application's decoded-event and multisig table defaults."
  type = object({
    event_fields    = optional(list(string))
    multisig_fields = optional(list(string))
  })
  default = {}

  validation {
    condition = var.dashboard.event_fields == null ? true : (
      length(var.dashboard.event_fields) > 0 &&
      length(distinct(var.dashboard.event_fields)) == length(var.dashboard.event_fields) &&
      alltrue([
        for field in var.dashboard.event_fields :
        can(regex("^[A-Za-z_][A-Za-z0-9_]*(\\.[A-Za-z_][A-Za-z0-9_]*)*$", field))
      ])
    )
    error_message = "dashboard.event_fields must contain unique dot-separated JSON field paths."
  }

  validation {
    condition = var.dashboard.multisig_fields == null ? true : (
      length(var.dashboard.multisig_fields) > 0 &&
      length(distinct(var.dashboard.multisig_fields)) == length(var.dashboard.multisig_fields) &&
      alltrue([
        for field in var.dashboard.multisig_fields :
        can(regex("^[A-Za-z_][A-Za-z0-9_]*(\\.[A-Za-z_][A-Za-z0-9_]*)*$", field))
      ])
    )
    error_message = "dashboard.multisig_fields must contain unique dot-separated JSON field paths."
  }
}

variable "alerting" {
  description = "Default Grafana evaluation timing applied to alerts without overrides."
  type = object({
    lookback_window_seconds     = optional(number, 60)
    evaluation_interval_seconds = optional(number, 10)
  })
  default = {}

  validation {
    condition = (
      var.alerting.lookback_window_seconds > 0 &&
      var.alerting.evaluation_interval_seconds > 0 &&
      var.alerting.evaluation_interval_seconds % 10 == 0 &&
      var.alerting.lookback_window_seconds >= var.alerting.evaluation_interval_seconds
    )
    error_message = "alerting timings must be positive, evaluation_interval_seconds must be a multiple of 10, and lookback_window_seconds must be at least evaluation_interval_seconds."
  }
}

variable "alerts" {
  description = "Program event, instruction, and Squads multisig alerts provisioned in Grafana."
  type = list(object({
    kind                        = optional(string, "event")
    name                        = string
    match                       = optional(string, "all")
    conditions                  = optional(any, [])
    severity                    = string
    channels                    = optional(set(string), [])
    lookback_window_seconds     = optional(number)
    evaluation_interval_seconds = optional(number)
  }))
  default = []

  validation {
    condition = alltrue([
      for alert in var.alerts :
      contains(["event", "instruction", "multisig"], alert.kind) &&
      length(trimspace(alert.name)) > 0 &&
      contains(["all", "any"], alert.match) &&
      contains(["critical", "error", "warning", "info"], alert.severity) &&
      alltrue([for channel in alert.channels : contains(["slack", "telegram", "pagerduty"], channel)]) &&
      try(alltrue([
        for condition in alert.conditions :
        can(regex("^[_A-Za-z][_0-9A-Za-z]*(\\.[_A-Za-z][_0-9A-Za-z]*)*$", condition.field)) &&
        contains(["exists", "contains", "eq", "ne", "gt", "gte", "lt", "lte"], condition.operator) &&
        (
          condition.operator == "exists" ? try(condition.value, null) == null :
          condition.operator == "contains" ? can(regex("^\".+\"$", jsonencode(try(condition.value, null)))) :
          contains(["gt", "gte", "lt", "lte"], condition.operator) ? can(tonumber(jsonencode(try(condition.value, null)))) :
          (
            can(regex("^\"", jsonencode(try(condition.value, null)))) ||
            can(tonumber(jsonencode(try(condition.value, null)))) ||
            contains(["true", "false"], jsonencode(try(condition.value, null)))
          )
        )
      ]), false) &&
      coalesce(alert.lookback_window_seconds, 1) > 0 &&
      coalesce(alert.evaluation_interval_seconds, 10) > 0 &&
      coalesce(alert.evaluation_interval_seconds, 10) % 10 == 0
    ])
    error_message = "alerts must use supported kinds, all/any matching, typed conditions, severities, channels, positive timing overrides, and evaluation intervals that are multiples of 10."
  }
}

variable "operators" {
  description = "IAM members granted SSH to the VM through IAP and OS Login, for example [\"user:alice@example.com\"]. Requires permission to set project IAM policy; leave empty when an administrator manages the grants (roles/compute.osAdminLogin, roles/iap.tunnelResourceAccessor, roles/compute.viewer) outside Terraform."
  type        = list(string)
  default     = []

  validation {
    condition     = alltrue([for member in var.operators : can(regex("^(user|group|serviceAccount):", member))])
    error_message = "operators entries must be IAM members prefixed with user:, group:, or serviceAccount:."
  }
}

variable "machine_type" {
  description = "Compute Engine machine type. Four GiB of memory is recommended for image builds."
  type        = string
  default     = "e2-standard-2"
}

variable "disk_size_gb" {
  description = "Boot disk size."
  type        = number
  default     = 40

  validation {
    condition     = var.disk_size_gb >= 30
    error_message = "disk_size_gb must be at least 30."
  }
}

variable "repository_url" {
  description = "Git repository cloned onto the VM."
  type        = string
  default     = "https://github.com/solana-foundation/solana-microscope.git"
}

variable "repository_deploy_key" {
  description = "Optional read-only OpenSSH deploy key used to clone a private repository."
  type        = string
  sensitive   = true
  default     = ""

  validation {
    condition = (
      var.repository_deploy_key == "" ||
      startswith(trimspace(var.repository_deploy_key), "-----BEGIN OPENSSH PRIVATE KEY-----")
    )
    error_message = "repository_deploy_key must be an OpenSSH private key."
  }
}

variable "repository_ref" {
  description = "Git branch, tag, or commit deployed onto the VM."
  type        = string
  default     = "main"
}
variable "prometheus_retention" {
  description = "How long the local-stack Prometheus keeps metrics."
  type        = string
  default     = "30d"

  validation {
    condition     = can(regex("^[0-9]+(ms|s|m|h|d|w|y)$", var.prometheus_retention))
    error_message = "prometheus_retention must be a Prometheus duration such as 30d or 720h."
  }
}
variable "stream_stale_after_seconds" {
  description = "How long the Yellowstone endpoint may fail its readiness probe before /readyz reports unready. 0 disables the check; null keeps the indexer default of 90."
  type        = number
  default     = null

  validation {
    condition     = var.stream_stale_after_seconds == null || (var.stream_stale_after_seconds >= 0 && floor(var.stream_stale_after_seconds) == var.stream_stale_after_seconds)
    error_message = "stream_stale_after_seconds must be a whole number of seconds, or 0 to disable the check."
  }
}
