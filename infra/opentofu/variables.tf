terraform {
  required_version = ">= 1.8.0"
}

variable "image_digest" {
  description = "Immutable OCI image digest shared by the API and worker deployments."
  type        = string
  validation {
    condition = can(regex("^sha256:[0-9a-f]{64}$", var.image_digest))
    error_message = "image_digest must be a sha256 OCI digest."
  }
}
variable "database_url_secret" {
  type = string
  sensitive = true
}
variable "address_encryption_key_secret" {
  type = string
  sensitive = true
}
variable "mavapay_secret" {
  type = string
  sensitive = true
}
variable "oidc_issuer" { type = string }
variable "proof_bucket" { type = string }
variable "public_base_url" { type = string }
variable "public_money_enabled" {
  description = "Technical staging must keep public-money collection disabled."
  type = bool
  default = false
  validation {
    condition = var.public_money_enabled == false
    error_message = "This cloud-agnostic staging interface never enables public-money collection."
  }
}
