variable "region" { type = string }
variable "name" { type = string  default = "openpool-staging" }
variable "image" { description = "Immutable image reference ending in @sha256:…" type = string }
variable "database_url_secret_arn" { type = string sensitive = true }
variable "address_encryption_key_secret_arn" { type = string sensitive = true }
variable "mavapay_secret_arn" { type = string sensitive = true }
variable "oidc_secret_arn" { type = string sensitive = true }
variable "proof_storage_secret_arn" { type = string sensitive = true }
variable "oidc_issuer" { type = string }
variable "vpc_id" { type = string }
variable "private_subnet_ids" { type = list(string) }
variable "security_group_ids" { type = list(string) }
variable "alert_email" { type = string }
variable "database_name" { type = string default = "openpool" }
variable "public_money_enabled" { type = bool default = false
  validation { condition = var.public_money_enabled == false error_message = "Technical staging never enables public-money collection." }
}
