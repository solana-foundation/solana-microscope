output "public_ip" {
  description = "Public address of the Microscope VM."
  value       = google_compute_address.this.address
}

output "grafana_tunnel" {
  description = "Open the tunnel, then browse to http://localhost:3000. Unused when grafana_cloud is set; the dashboards live in the hosted stack."
  value       = "gcloud compute ssh ${google_compute_instance.this.name} --tunnel-through-iap --zone ${var.zone} --project ${var.project_id} -- -L 3000:127.0.0.1:3000 -N"
}

output "bootstrap_log" {
  description = "Command for following initial and subsequent deployment reconciliations."
  value       = "gcloud compute ssh ${google_compute_instance.this.name} --tunnel-through-iap --zone ${var.zone} --project ${var.project_id} --command 'sudo tail -f /var/log/microscope-deploy.log'"
}
