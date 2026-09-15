locals {
  idl_file = "${path.module}/${var.idl_path}"
  labels = {
    application = "solana-microscope"
    managed-by  = "terraform"
  }

  alert_channels = toset(flatten([
    for alert in var.alerts : tolist(alert.channels)
  ]))
  invalid_alert_timings = [
    for alert in var.alerts : alert.name
    if coalesce(alert.lookback_window_seconds, var.alerting.lookback_window_seconds) < coalesce(alert.evaluation_interval_seconds, var.alerting.evaluation_interval_seconds)
  ]
  microscope_config = templatefile("${path.module}/../microscope.toml.tftpl", {
    program_id = var.program_id
    multisig   = var.multisig
    datasource = var.datasource
    dashboard  = var.dashboard
    alerting   = var.alerting
    alerts     = var.alerts
  })

  deployment_config_object   = "microscope.toml"
  deployment_idl_object      = "program.json"
  deployment_manifest_object = "deployment.json"
  deployment_revision_inputs = {
    config_sha256  = sha256(local.microscope_config)
    idl_sha256     = filesha256(local.idl_file)
    repository_ref = var.repository_ref
    repository_url = var.repository_url
    secret_version = tostring(google_secret_manager_secret_version.runtime.version)
  }
  deployment_revision = sha256(jsonencode(local.deployment_revision_inputs))
  deployment_manifest = jsonencode({
    schema_version = 1
    revision       = local.deployment_revision
    repository_url = var.repository_url
    repository_ref = var.repository_ref
    config_object  = local.deployment_config_object
    config_sha256  = sha256(local.microscope_config)
    idl_object     = local.deployment_idl_object
    idl_sha256     = filesha256(local.idl_file)
    secret_version = tostring(google_secret_manager_secret_version.runtime.version)
  })

  user_data = templatefile("${path.module}/cloud-init.yaml.tftpl", {
    cloud_provider_b64      = base64encode("gcp")
    deployment_bucket_b64   = base64encode(google_storage_bucket.idl.name)
    deployment_manifest_b64 = base64encode(local.deployment_manifest_object)
    runtime_secret_b64      = base64encode(google_secret_manager_secret.runtime.secret_id)
    gcp_project_id_b64      = base64encode(var.project_id)
    deploy_script_b64       = filebase64("${path.module}/../deploy-microscope.sh")
  })
}

resource "google_project_service" "compute" {
  service            = "compute.googleapis.com"
  disable_on_destroy = false
}

resource "google_project_service" "secret_manager" {
  service            = "secretmanager.googleapis.com"
  disable_on_destroy = false
}

resource "google_project_service" "storage" {
  service            = "storage.googleapis.com"
  disable_on_destroy = false
}

resource "google_project_service" "iam" {
  service            = "iam.googleapis.com"
  disable_on_destroy = false
}

resource "google_project_service" "iap" {
  service            = "iap.googleapis.com"
  disable_on_destroy = false
}

resource "google_compute_network" "this" {
  name                    = var.name
  auto_create_subnetworks = false

  depends_on = [google_project_service.compute]
}

resource "google_compute_subnetwork" "this" {
  name          = var.name
  ip_cidr_range = "10.42.1.0/24"
  region        = var.region
  network       = google_compute_network.this.id
}

resource "google_compute_firewall" "ssh" {
  name      = "${var.name}-ssh"
  network   = google_compute_network.this.name
  direction = "INGRESS"

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }

  # Google's IAP TCP forwarding range; SSH is reachable only through the tunnel.
  source_ranges = ["35.235.240.0/20"]
  target_tags   = [var.name]
}

resource "google_compute_address" "this" {
  name   = var.name
  region = var.region

  depends_on = [google_project_service.compute]
}

resource "google_storage_bucket" "idl" {
  name                        = "${substr(format("%s-%s", var.project_id, var.name), 0, 59)}-idl"
  location                    = var.region
  force_destroy               = true
  uniform_bucket_level_access = true
  labels                      = local.labels

  versioning {
    enabled = true
  }

  depends_on = [google_project_service.storage]
}

resource "google_storage_bucket_object" "idl" {
  name         = local.deployment_idl_object
  bucket       = google_storage_bucket.idl.name
  source       = local.idl_file
  content_type = "application/json"
}

resource "google_storage_bucket_object" "config" {
  name         = local.deployment_config_object
  bucket       = google_storage_bucket.idl.name
  content      = local.microscope_config
  content_type = "application/toml"
}

resource "google_secret_manager_secret" "runtime" {
  secret_id = "${var.name}-runtime"

  replication {
    auto {}
  }

  labels     = local.labels
  depends_on = [google_project_service.secret_manager]
}

resource "google_secret_manager_secret_version" "runtime" {
  secret = google_secret_manager_secret.runtime.id
  secret_data = jsonencode({
    geyser_url                 = var.geyser_url
    geyser_x_token             = var.geyser_x_token
    rpc_url                    = var.rpc_url
    grafana_admin_password     = var.grafana_admin_password
    slack_webhook_url          = var.slack_webhook_url
    telegram_bot_token         = var.telegram_bot_token
    telegram_chat_id           = var.telegram_chat_id
    pagerduty_integration_key  = var.pagerduty_integration_key
    repository_deploy_key      = var.repository_deploy_key
    grafana_cloud_loki_url     = try(var.grafana_cloud.loki_url, "")
    grafana_cloud_loki_user    = try(var.grafana_cloud.loki_user, "")
    grafana_cloud_prom_url     = try(var.grafana_cloud.prom_url, "")
    grafana_cloud_prom_user    = try(var.grafana_cloud.prom_user, "")
    grafana_cloud_token        = try(var.grafana_cloud.token, "")
    microscope_deployment      = try(var.grafana_cloud.deployment_name, "")
    microscope_env             = try(var.grafana_cloud.environment, "")
    prometheus_retention       = var.prometheus_retention
    stream_stale_after_seconds = var.stream_stale_after_seconds
  })

  lifecycle {
    precondition {
      condition     = var.datasource.mode != "yellowstone" || length(trimspace(var.geyser_url)) > 0
      error_message = "geyser_url is required when datasource.mode is yellowstone."
    }
    precondition {
      condition     = var.datasource.mode != "rpc" || length(trimspace(var.rpc_url)) > 0
      error_message = "rpc_url is required when datasource.mode is rpc."
    }
    precondition {
      condition     = var.grafana_cloud != null || length(var.grafana_admin_password) >= 12
      error_message = "grafana_admin_password is required unless grafana_cloud is set."
    }
    precondition {
      condition     = var.grafana_cloud != null || !contains(local.alert_channels, "slack") || length(trimspace(var.slack_webhook_url)) > 0
      error_message = "slack_webhook_url is required because an alert uses the slack channel."
    }
    precondition {
      condition = (
        var.grafana_cloud != null ||
        !contains(local.alert_channels, "telegram") ||
        (length(trimspace(var.telegram_bot_token)) > 0 && length(trimspace(var.telegram_chat_id)) > 0)
      )
      error_message = "telegram_bot_token and telegram_chat_id are required because an alert uses the telegram channel."
    }
    precondition {
      condition     = var.grafana_cloud != null || !contains(local.alert_channels, "pagerduty") || length(trimspace(var.pagerduty_integration_key)) > 0
      error_message = "pagerduty_integration_key is required because an alert uses the pagerduty channel."
    }
  }
}

resource "google_storage_bucket_object" "manifest" {
  name         = local.deployment_manifest_object
  bucket       = google_storage_bucket.idl.name
  content      = local.deployment_manifest
  content_type = "application/json"

  depends_on = [
    google_storage_bucket_object.config,
    google_storage_bucket_object.idl,
  ]
}


resource "google_project_iam_member" "operator_os_login" {
  for_each = toset(var.operators)

  project = var.project_id
  role    = "roles/compute.osAdminLogin"
  member  = each.value
}

resource "google_project_iam_member" "operator_iap" {
  for_each = toset(var.operators)

  project = var.project_id
  role    = "roles/iap.tunnelResourceAccessor"
  member  = each.value

  depends_on = [google_project_service.iap]
}

resource "google_project_iam_member" "operator_viewer" {
  for_each = toset(var.operators)

  project = var.project_id
  role    = "roles/compute.viewer"
  member  = each.value
}

resource "google_service_account" "instance" {
  account_id   = substr("microscope-${replace(var.name, "-", "")}", 0, 30)
  display_name = "Solana Microscope VM"

  depends_on = [google_project_service.iam]
}

resource "google_secret_manager_secret_iam_member" "instance" {
  secret_id = google_secret_manager_secret.runtime.id
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.instance.email}"
}

resource "google_storage_bucket_iam_member" "instance" {
  bucket = google_storage_bucket.idl.name
  role   = "roles/storage.objectViewer"
  member = "serviceAccount:${google_service_account.instance.email}"
}

data "google_compute_image" "ubuntu" {
  family  = "ubuntu-2404-lts-amd64"
  project = "ubuntu-os-cloud"

  depends_on = [google_project_service.compute]
}

resource "google_compute_instance" "this" {
  name         = var.name
  machine_type = var.machine_type
  zone         = var.zone
  tags         = [var.name]
  labels       = local.labels

  boot_disk {
    initialize_params {
      image = data.google_compute_image.ubuntu.self_link
      size  = var.disk_size_gb
      type  = "pd-balanced"
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.this.id

    access_config {
      nat_ip = google_compute_address.this.address
    }
  }

  metadata = {
    enable-oslogin = "TRUE"
    user-data      = local.user_data
  }

  service_account {
    email  = google_service_account.instance.email
    scopes = ["cloud-platform"]
  }

  shielded_instance_config {
    enable_integrity_monitoring = true
    enable_secure_boot          = true
    enable_vtpm                 = true
  }

  allow_stopping_for_update = true

  lifecycle {
    # Keep routine config applies from replacing the VM when the Ubuntu image
    # family advances. A deliberate replacement still resolves the latest image.
    ignore_changes = [boot_disk[0].initialize_params[0].image]

    precondition {
      condition = (
        var.repository_deploy_key == "" ||
        startswith(var.repository_url, "git@") ||
        startswith(var.repository_url, "ssh://")
      )
      error_message = "repository_url must be an SSH URL when repository_deploy_key is set."
    }
    precondition {
      condition     = startswith(var.zone, "${var.region}-")
      error_message = "zone must belong to region."
    }
    precondition {
      condition     = length(local.invalid_alert_timings) == 0
      error_message = "Each alert lookback must be at least its effective evaluation interval. Invalid alerts: ${join(", ", local.invalid_alert_timings)}."
    }
    precondition {
      condition = (
        var.multisig != null ||
        !anytrue([for alert in var.alerts : alert.kind == "multisig"])
      )
      error_message = "multisig alerts require multisig to be set."
    }
  }

  depends_on = [
    google_secret_manager_secret_iam_member.instance,
    google_storage_bucket_iam_member.instance,
    google_storage_bucket_object.manifest,
  ]
}
