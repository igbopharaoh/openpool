output "release_contract" {
  description = "Inputs that a cloud-specific module must consume for an OpenPool technical-staging release."
  value = {
    image_digest = var.image_digest
    oidc_issuer = var.oidc_issuer
    proof_bucket = var.proof_bucket
    public_base_url = var.public_base_url
    public_money_enabled = var.public_money_enabled
  }
}
