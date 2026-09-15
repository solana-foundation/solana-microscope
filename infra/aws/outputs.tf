output "public_ip" {
  description = "Public address of the Microscope VM."
  value       = aws_instance.this.public_ip
}

output "grafana_tunnel" {
  description = "Open the tunnel, then browse to http://localhost:3000. Unused when grafana_cloud is set; the dashboards live in the hosted stack."
  value       = "aws ssm start-session --target ${aws_instance.this.id} --document-name AWS-StartPortForwardingSession --parameters '${jsonencode({ portNumber = ["3000"], localPortNumber = ["3000"] })}' --region ${var.region}"
}

output "bootstrap_log" {
  description = "Command for following initial and subsequent deployment reconciliations."
  value       = "aws ssm start-session --target ${aws_instance.this.id} --document-name AWS-StartInteractiveCommand --parameters '${jsonencode({ command = ["sudo tail -f /var/log/microscope-deploy.log"] })}' --region ${var.region}"
}
