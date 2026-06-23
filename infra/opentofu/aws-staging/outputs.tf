output "proof_bucket" { value = aws_s3_bucket.proofs.bucket }
output "cluster" { value = aws_ecs_cluster.this.name }
output "migration_task_definition" { value = aws_ecs_task_definition.migrate.arn }
output "alert_topic" { value = aws_sns_topic.alerts.arn }
