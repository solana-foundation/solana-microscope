mock_provider "google" {}

variables {
  project_id             = "microscope-test"
  geyser_url             = "https://example.invalid:443"
  geyser_x_token         = "token"
  grafana_admin_password = "a-strong-password-for-tests"
}

run "one_program" {
  command = apply

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
  }

  assert {
    condition     = length(google_storage_bucket_object.idl) == 1 && google_storage_bucket_object.idl[0].name == "program.json"
    error_message = "one program keeps the single program.json object"
  }
  assert {
    condition     = startswith(local.microscope_config, "program_id = \"De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44\"\nidl_path = \"idl/program.json\"\n")
    error_message = "one program renders the top-level keys"
  }
  assert {
    condition     = !can(jsondecode(local.deployment_manifest).extra_idls)
    error_message = "one program adds nothing to the manifest"
  }
}

run "several_programs" {
  command = apply

  variables {
    programs = [
      { program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44", idl_path = "../../idl/idl1.json" },
      { program_id = "SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf", idl_path = "../../idl/squads/v4.json" },
    ]
    alerts = [
      { kind = "instruction", name = "subscribe", severity = "info", program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44" },
    ]
  }

  assert {
    condition     = [for object in google_storage_bucket_object.idl : object.name] == ["program.json", "program-1.json"]
    error_message = "every program gets its own IDL object"
  }
  assert {
    condition     = strcontains(local.microscope_config, "[[programs]]\nprogram_id = \"SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf\"\nidl_path = \"idl/program-1.json\"\n")
    error_message = "every program renders a [[programs]] table that points at its installed IDL"
  }
  assert {
    condition = jsondecode(local.deployment_manifest).extra_idls == [
      { object = "program-1.json", sha256 = filesha256("../../idl/squads/v4.json") },
    ]
    error_message = "the manifest lists every extra IDL with its digest"
  }
}

run "rejects_program_id_with_programs" {
  command = plan

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    programs = [
      { program_id = "SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf", idl_path = "../../idl/squads/v4.json" },
    ]
  }

  expect_failures = [google_compute_instance.this]
}

run "rejects_an_alert_on_an_unconfigured_program" {
  command = plan

  variables {
    program_id = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44"
    alerts = [
      { kind = "instruction", name = "subscribe", severity = "info", program_id = "SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf" },
    ]
  }

  expect_failures = [google_compute_instance.this]
}
