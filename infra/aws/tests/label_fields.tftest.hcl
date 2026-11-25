mock_provider "aws" {
  mock_data "aws_ami" {
    defaults = {
      id = "ami-00000000000000000"
    }
  }
  mock_data "aws_iam_policy_document" {
    defaults = {
      json = "{\"Version\":\"2012-10-17\",\"Statement\":[]}"
    }
  }
}

variables {
  region                 = "us-east-2"
  geyser_url             = "https://example.invalid:443"
  geyser_x_token         = "token"
  grafana_admin_password = "a-strong-password-for-tests"
  telegram_bot_token     = "000000:test"
  telegram_chat_id       = "-100000"
}

run "renders_label_fields" {
  command = apply

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    alerts = [
      { kind = "instruction", name = "subscribe", severity = "info", channels = ["telegram"], label_fields = ["data.group", "instruction"] },
      { kind = "instruction", name = "cancel", severity = "info", channels = ["telegram"] },
    ]
  }

  assert {
    condition     = strcontains(local.microscope_config, "channels = [\"telegram\"]\nlabel_fields = [\"data.group\",\"instruction\"]\n")
    error_message = "an alert's label_fields render in its [[alerts]] table"
  }
  assert {
    condition     = length(regexall("label_fields = ", local.microscope_config)) == 1
    error_message = "an alert without label_fields renders none"
  }
}

run "rejects_a_malformed_label_field" {
  command = plan

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    alerts = [
      { kind = "instruction", name = "subscribe", severity = "info", label_fields = ["data..group"] },
    ]
  }

  expect_failures = [var.alerts]
}

run "rejects_more_than_five_label_fields" {
  command = plan

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    alerts = [
      { kind = "instruction", name = "subscribe", severity = "info", label_fields = ["a", "b", "c", "d", "e", "f"] },
    ]
  }

  expect_failures = [var.alerts]
}

run "rejects_label_fields_that_become_the_same_label" {
  command = plan

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    alerts = [
      { kind = "instruction", name = "subscribe", severity = "info", label_fields = ["data.a_b", "data.a.b"] },
    ]
  }

  expect_failures = [var.alerts]
}
