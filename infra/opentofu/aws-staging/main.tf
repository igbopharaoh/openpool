locals {
  tags = { Application = "openpool", Environment = "technical-staging", ManagedBy = "opentofu" }
  secret_arns = [var.database_url_secret_arn, var.address_encryption_key_secret_arn, var.mavapay_secret_arn, var.oidc_secret_arn, var.proof_storage_secret_arn]
}

resource "aws_s3_bucket" "proofs" {
  bucket_prefix = "${var.name}-proofs-"
  object_lock_enabled = true
  force_destroy = false
  tags = local.tags
}
resource "aws_s3_bucket_versioning" "proofs" { bucket = aws_s3_bucket.proofs.id versioning_configuration { status = "Enabled" } }
resource "aws_s3_bucket_object_lock_configuration" "proofs" {
  bucket = aws_s3_bucket.proofs.id
  rule { default_retention { mode = "COMPLIANCE" days = 2555 } }
}
resource "aws_s3_bucket_server_side_encryption_configuration" "proofs" {
  bucket = aws_s3_bucket.proofs.id
  rule { apply_server_side_encryption_by_default { sse_algorithm = "AES256" } }
}
resource "aws_cloudwatch_log_group" "api" { name = "/openpool/${var.name}/api" retention_in_days = 90 tags = local.tags }
resource "aws_cloudwatch_log_group" "worker" { name = "/openpool/${var.name}/worker" retention_in_days = 90 tags = local.tags }
resource "aws_ecs_cluster" "this" { name = var.name tags = local.tags }
resource "aws_db_subnet_group" "postgres" { name_prefix = "${var.name}-" subnet_ids = var.private_subnet_ids tags = local.tags }
resource "aws_db_instance" "postgres" {
  identifier_prefix = "${var.name}-" engine = "postgres" engine_version = "16" instance_class = "db.t4g.medium"
  allocated_storage = 30 max_allocated_storage = 100 storage_encrypted = true db_name = var.database_name
  username = "openpool" manage_master_user_password = true db_subnet_group_name = aws_db_subnet_group.postgres.name
  vpc_security_group_ids = var.security_group_ids backup_retention_period = 14 copy_tags_to_snapshot = true
  deletion_protection = true skip_final_snapshot = false publicly_accessible = false multi_az = false
  tags = local.tags
}

data "aws_iam_policy_document" "task_assume" { statement { actions = ["sts:AssumeRole"] principals { type = "Service" identifiers = ["ecs-tasks.amazonaws.com"] } } }
resource "aws_iam_role" "task" { name_prefix = "${var.name}-task-" assume_role_policy = data.aws_iam_policy_document.task_assume.json tags = local.tags }
resource "aws_iam_role_policy" "task" {
  role = aws_iam_role.task.id
  policy = jsonencode({ Version = "2012-10-17", Statement = [
    { Effect = "Allow", Action = ["secretsmanager:GetSecretValue"], Resource = local.secret_arns },
    { Effect = "Allow", Action = ["s3:PutObject", "s3:PutObjectRetention", "s3:GetObjectVersion"], Resource = ["${aws_s3_bucket.proofs.arn}/*"] }
  ]})
}

resource "aws_ecs_task_definition" "api" {
  family = "${var.name}-api" requires_compatibilities = ["FARGATE"] network_mode = "awsvpc" cpu = 512 memory = 1024 execution_role_arn = aws_iam_role.task.arn task_role_arn = aws_iam_role.task.arn
  container_definitions = jsonencode([{ name = "api", image = var.image, essential = true, command = ["openpool-api-app"], environment = [{ name = "APP_ENV", value = "staging" }, { name = "PUBLIC_MONEY_ENABLED", value = "false" }, { name = "OIDC_ISSUER", value = var.oidc_issuer }, { name = "PROOF_STORAGE_BUCKET", value = aws_s3_bucket.proofs.bucket }], logConfiguration = { logDriver = "awslogs", options = { awslogs-group = aws_cloudwatch_log_group.api.name, awslogs-region = var.region, awslogs-stream-prefix = "api" } } }])
  tags = local.tags
}
resource "aws_ecs_task_definition" "worker" {
  family = "${var.name}-worker" requires_compatibilities = ["FARGATE"] network_mode = "awsvpc" cpu = 512 memory = 1024 execution_role_arn = aws_iam_role.task.arn task_role_arn = aws_iam_role.task.arn
  container_definitions = jsonencode([{ name = "worker", image = var.image, essential = true, command = ["openpool-worker-app"], environment = [{ name = "APP_ENV", value = "staging" }, { name = "PUBLIC_MONEY_ENABLED", value = "false" }, { name = "PROOF_STORAGE_BUCKET", value = aws_s3_bucket.proofs.bucket }], logConfiguration = { logDriver = "awslogs", options = { awslogs-group = aws_cloudwatch_log_group.worker.name, awslogs-region = var.region, awslogs-stream-prefix = "worker" } } }])
  tags = local.tags
}

# Migrations are an explicit one-off task, invoked by the deployment workflow before API/worker rollout.
resource "aws_ecs_task_definition" "migrate" {
  family = "${var.name}-migrate" requires_compatibilities = ["FARGATE"] network_mode = "awsvpc" cpu = 256 memory = 512 execution_role_arn = aws_iam_role.task.arn task_role_arn = aws_iam_role.task.arn
  container_definitions = jsonencode([{ name = "migrate", image = var.image, essential = true, command = ["openpool-api-app"], environment = [{ name = "OPENPOOL_MIGRATE_ONLY", value = "true" }, { name = "APP_ENV", value = "staging" }], logConfiguration = { logDriver = "awslogs", options = { awslogs-group = aws_cloudwatch_log_group.api.name, awslogs-region = var.region, awslogs-stream-prefix = "migrate" } } }])
  tags = local.tags
}
resource "aws_sns_topic" "alerts" { name = "${var.name}-alerts" tags = local.tags }
resource "aws_sns_topic_subscription" "alerts" { topic_arn = aws_sns_topic.alerts.arn protocol = "email" endpoint = var.alert_email }
resource "aws_cloudwatch_metric_alarm" "worker_errors" { alarm_name = "${var.name}-worker-errors" comparison_operator = "GreaterThanOrEqualToThreshold" evaluation_periods = 1 metric_name = "ErrorCount" namespace = "AWS/Logs" period = 300 statistic = "Sum" threshold = 1 alarm_actions = [aws_sns_topic.alerts.arn] dimensions = { LogGroupName = aws_cloudwatch_log_group.worker.name } }
