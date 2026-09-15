locals {
  common_tags = {
    Application = "solana-microscope"
    ManagedBy   = "terraform"
  }

  idl_file = "${path.module}/${var.idl_path}"
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
  deployment_config_key   = "microscope.toml"
  deployment_idl_key      = "program.json"
  deployment_manifest_key = "deployment.json"
  deployment_revision_inputs = {
    config_sha256  = sha256(local.microscope_config)
    idl_sha256     = filesha256(local.idl_file)
    repository_ref = var.repository_ref
    repository_url = var.repository_url
    secret_version = tostring(aws_secretsmanager_secret_version.runtime.version_id)
  }
  deployment_revision = sha256(jsonencode(local.deployment_revision_inputs))
  deployment_manifest = jsonencode({
    schema_version = 1
    revision       = local.deployment_revision
    repository_url = var.repository_url
    repository_ref = var.repository_ref
    config_object  = local.deployment_config_key
    config_sha256  = sha256(local.microscope_config)
    idl_object     = local.deployment_idl_key
    idl_sha256     = filesha256(local.idl_file)
    secret_version = tostring(aws_secretsmanager_secret_version.runtime.version_id)
  })
  user_data = templatefile("${path.module}/cloud-init.yaml.tftpl", {
    cloud_provider_b64      = base64encode("aws")
    deployment_bucket_b64   = base64encode(aws_s3_bucket.idl.bucket)
    deployment_manifest_b64 = base64encode(local.deployment_manifest_key)
    runtime_secret_b64      = base64encode(aws_secretsmanager_secret.runtime.arn)
    aws_region_b64          = base64encode(var.region)
    deploy_script_b64       = filebase64("${path.module}/../deploy-microscope.sh")
  })
}

data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"]

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*"]
  }

  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

resource "aws_vpc" "this" {
  cidr_block           = "10.42.0.0/16"
  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = merge(local.common_tags, { Name = var.name })
}

resource "aws_internet_gateway" "this" {
  vpc_id = aws_vpc.this.id
  tags   = merge(local.common_tags, { Name = var.name })
}

resource "aws_subnet" "public" {
  vpc_id                  = aws_vpc.this.id
  cidr_block              = "10.42.1.0/24"
  map_public_ip_on_launch = true

  tags = merge(local.common_tags, { Name = "${var.name}-public" })
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.this.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.this.id
  }

  tags = merge(local.common_tags, { Name = "${var.name}-public" })
}

resource "aws_route_table_association" "public" {
  subnet_id      = aws_subnet.public.id
  route_table_id = aws_route_table.public.id
}

resource "aws_security_group" "instance" {
  name_prefix = "${var.name}-"
  description = "Egress-only; operators connect through SSM Session Manager"
  vpc_id      = aws_vpc.this.id

  egress {
    description = "Outbound package, image, Git, and Solana datasource access"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = local.common_tags
}

resource "aws_s3_bucket" "idl" {
  bucket_prefix = "${var.name}-idl-"
  force_destroy = true
  tags          = local.common_tags
}

resource "aws_s3_bucket_public_access_block" "idl" {
  bucket                  = aws_s3_bucket.idl.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "idl" {
  bucket = aws_s3_bucket.idl.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_versioning" "idl" {
  bucket = aws_s3_bucket.idl.id

  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_object" "idl" {
  bucket                 = aws_s3_bucket.idl.id
  key                    = local.deployment_idl_key
  source                 = local.idl_file
  etag                   = filemd5(local.idl_file)
  server_side_encryption = "AES256"
  content_type           = "application/json"

  depends_on = [aws_s3_bucket_versioning.idl]
}

resource "aws_s3_object" "config" {
  bucket                 = aws_s3_bucket.idl.id
  key                    = local.deployment_config_key
  content                = local.microscope_config
  etag                   = md5(local.microscope_config)
  server_side_encryption = "AES256"
  content_type           = "application/toml"

  depends_on = [aws_s3_bucket_versioning.idl]
}

resource "aws_secretsmanager_secret" "runtime" {
  name_prefix             = "${var.name}-runtime-"
  recovery_window_in_days = 0
  tags                    = local.common_tags
}

resource "aws_secretsmanager_secret_version" "runtime" {
  secret_id = aws_secretsmanager_secret.runtime.id
  secret_string = jsonencode({
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

resource "aws_s3_object" "manifest" {
  bucket                 = aws_s3_bucket.idl.id
  key                    = local.deployment_manifest_key
  content                = local.deployment_manifest
  etag                   = md5(local.deployment_manifest)
  server_side_encryption = "AES256"
  content_type           = "application/json"

  depends_on = [
    aws_s3_object.config,
    aws_s3_object.idl,
  ]
}

resource "aws_iam_role" "instance" {
  name_prefix = "${var.name}-"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Principal = {
        Service = "ec2.amazonaws.com"
      }
      Action = "sts:AssumeRole"
    }]
  })
  tags = local.common_tags
}

resource "aws_iam_role_policy" "bootstrap" {
  name = "bootstrap"
  role = aws_iam_role.instance.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["s3:GetObject"]
        Resource = ["${aws_s3_bucket.idl.arn}/*"]
      },
      {
        Effect   = "Allow"
        Action   = ["secretsmanager:GetSecretValue"]
        Resource = [aws_secretsmanager_secret.runtime.arn]
      }
    ]
  })
}

resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.instance.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_instance_profile" "instance" {
  name_prefix = "${var.name}-"
  role        = aws_iam_role.instance.name
  tags        = local.common_tags
}

resource "aws_instance" "this" {
  ami                         = data.aws_ami.ubuntu.id
  instance_type               = var.instance_type
  subnet_id                   = aws_subnet.public.id
  associate_public_ip_address = true
  vpc_security_group_ids      = [aws_security_group.instance.id]
  iam_instance_profile        = aws_iam_instance_profile.instance.name
  user_data                   = local.user_data
  user_data_replace_on_change = true

  # IMDSv2 with a single hop denies containers the instance role credentials;
  # the GCP deployment's microscope-metadata-guard unit does this with iptables.
  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  root_block_device {
    encrypted   = true
    volume_size = var.disk_size_gb
    volume_type = "gp3"
  }

  lifecycle {
    # Keep routine config applies from replacing the VM when Canonical publishes
    # a newer image. A deliberate replacement still resolves the latest image.
    ignore_changes = [ami]

    precondition {
      condition = (
        var.repository_deploy_key == "" ||
        startswith(var.repository_url, "git@") ||
        startswith(var.repository_url, "ssh://")
      )
      error_message = "repository_url must be an SSH URL when repository_deploy_key is set."
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
    aws_iam_role_policy.bootstrap,
    aws_route_table_association.public,
    aws_s3_object.manifest,
  ]

  tags = merge(local.common_tags, { Name = var.name })
}
