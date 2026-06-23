# AWS technical-staging module

This is the concrete AWS implementation for technical staging. It creates an Object-Lock-enabled,
versioned proof bucket; ECS task definitions for API, worker, and an explicit migration task;
least-privilege task IAM; CloudWatch retention; and an SNS-backed worker-error alarm.

It intentionally does **not** create a public load balancer, DNS record, or public-money feature.
Pass existing private subnets/security groups and Secrets Manager ARNs from the selected account.
Run `tofu plan` and have an operator confirm the Object Lock and retention settings before apply:
Object Lock cannot be disabled later.
