mock_provider "google" {}

variables {
  project_id             = "microscope-test"
  geyser_url             = "https://example.invalid:443"
  geyser_x_token         = "token"
  grafana_admin_password = "a-strong-password-for-tests"
  slack_webhook_url      = "https://hooks.slack.com/services/T000/B000/test"
}

run "renders_slack_mentions" {
  command = apply

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    alerting   = { health_slack_mention = "@here" }
    alerts = [
      { kind = "instruction", name = "subscribe", severity = "info", channels = ["slack"], slack_mention = "U0123ABCDEF" },
      { kind = "instruction", name = "cancel", severity = "info", channels = ["slack"] },
    ]
  }

  assert {
    condition     = strcontains(local.microscope_config, "health_slack_mention = \"@here\"\n")
    error_message = "the health mention renders under [alerting]"
  }
  assert {
    condition     = strcontains(local.microscope_config, "channels = [\"slack\"]\nslack_mention = \"U0123ABCDEF\"\n")
    error_message = "an alert's mention renders in its [[alerts]] table"
  }
  assert {
    condition     = length(regexall("slack_mention = ", local.microscope_config)) == 2
    error_message = "an alert without a mention renders none"
  }
}

run "rejects_a_malformed_slack_mention" {
  command = plan

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    alerts = [
      { kind = "instruction", name = "subscribe", severity = "info", channels = ["slack"], slack_mention = "here" },
    ]
  }

  expect_failures = [var.alerts]
}

run "rejects_a_slack_mention_without_the_slack_channel" {
  command = plan

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    alerts = [
      { kind = "instruction", name = "subscribe", severity = "info", channels = ["telegram"], slack_mention = "@here" },
    ]
  }

  expect_failures = [var.alerts]
}

run "rejects_a_malformed_health_slack_mention" {
  command = plan

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    alerting   = { health_slack_mention = "<@U0123ABCDEF>" }
  }

  expect_failures = [var.alerting]
}

run "rejects_an_empty_health_slack_mention" {
  command = plan

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    alerting   = { health_slack_mention = "" }
  }

  expect_failures = [var.alerting]
}

run "rejects_a_health_slack_mention_without_a_slack_alert" {
  command = plan

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    alerting   = { health_slack_mention = "@here" }
    alerts = [
      { kind = "instruction", name = "subscribe", severity = "info", channels = [] },
    ]
  }

  expect_failures = [google_compute_instance.this]
}
