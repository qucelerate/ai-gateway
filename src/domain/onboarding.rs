//! Cloud onboarding: what a tenant creates in its own cloud so the gateway
//! can reach its models without a long-lived key, rendered for one
//! organization.
//!
//! The gateway's own identities come from configuration
//! ([`OnboardingIdentities`]); the instructions name them and the
//! organization's AWS external ID, and nothing secret. Each cloud is offered
//! as a one-click link where the cloud has one, its CLI, and Terraform:
//!
//! - **AWS:** a role named [`AWS_ROLE_NAME`] that trusts the gateway's
//!   principal with the organization's external ID and may only invoke and
//!   list Bedrock models (CloudFormation quick-create of the published
//!   template, AWS CLI, Terraform). The role's name is fixed, so its ARN
//!   follows from the account ID. Opted in, it may also accept models'
//!   AWS Marketplace agreements (D-035): [`BEDROCK_AGREEMENT_ACTIONS`].
//! - **Azure:** admin consent to the gateway's multi-tenant app, then a
//!   role for it on the tenant's resource (`az`, Terraform).
//! - **Google Cloud:** a service account in the tenant's project with
//!   `roles/aiplatform.user`, which the gateway may impersonate
//!   (`gcloud`, Cloud Shell, Terraform).
//!
//! Each may also be set up for native batches (D-021), off unless asked
//! for: on AWS, given the staging bucket and the batch service role, the
//! role may run batch inference jobs ([`BEDROCK_BATCH_ACTIONS`]); on Azure
//! OpenAI, a custom role lets the app upload batch files and create and
//! cancel batch jobs ([`AZURE_BATCH_DATA_ACTIONS`]); on Google Cloud, given
//! the staging bucket, the service account and the project's Vertex AI
//! service agent may read and write it.

use serde::Serialize;
use url::Url;

use super::identity::OrganizationId;

/// The role the AWS instructions create in a tenant's account.
pub const AWS_ROLE_NAME: &str = "QucelerateGateway";
/// The service account the Google instructions create in a tenant's project.
pub const GOOGLE_ACCOUNT_ID: &str = "qucelerate-gateway";

/// The gateway's own cloud identities, as tenants must name them. A cloud
/// whose identity is not configured is not offered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OnboardingIdentities {
    /// Origins of OpenAI-compatible endpoints tenants may connect over plain
    /// HTTP or at private addresses (the operator's allowlist). Any tenant
    /// may connect a public HTTPS endpoint, so that kind is always offered.
    pub openai_compatible_private_endpoints: Vec<String>,
    /// The AWS principal tenant roles trust (a user's or role's ARN).
    pub aws_principal_arn: Option<String>,
    /// The published CloudFormation template (`aws-bedrock-role-v1.yaml`).
    pub aws_template_url: Option<Url>,
    /// The gateway's multi-tenant Entra app (client) ID.
    pub azure_client_id: Option<String>,
    /// Where Microsoft returns an administrator after consent.
    pub azure_consent_redirect: Option<Url>,
    /// The gateway's Google service account.
    pub google_service_account: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OnboardingError {
    #[error("this cloud is not configured for onboarding")]
    Unavailable,
    #[error("account_id must be a 12-digit AWS account ID")]
    AwsAccount,
    #[error("region must be an AWS region such as us-east-1")]
    AwsRegion,
    #[error(
        "resource_id must be an Azure resource ID of a Cognitive Services account, and family azure-openai or azure-ai-foundry"
    )]
    AzureResource,
    #[error("project_id must be a Google Cloud project ID")]
    GoogleProject,
    #[error(
        "batch_bucket must be an S3 bucket name and batch_role_arn an IAM role ARN in the same account and partition; give both or neither"
    )]
    AwsBatch,
    #[error("batch applies to family azure-openai only: Azure AI Foundry runs no batch jobs")]
    AzureBatch,
    #[error("batch_bucket must be a Cloud Storage bucket name")]
    GoogleBatch,
}

/// What a tenant names to let its AWS role run batch inference jobs: the
/// staging bucket and the service role Bedrock assumes to read and write it
/// (docs/operations.md, *Batch staging*).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwsBatch<'a> {
    pub bucket: &'a str,
    pub service_role_arn: &'a str,
}

/// Which clouds are offered, and the identities they name.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OnboardingSummary {
    pub aws: Option<AwsIdentity>,
    pub azure: Option<AzureIdentity>,
    pub google: Option<GoogleIdentity>,
    /// Always offered: needs nothing from the gateway's own clouds.
    pub openai_compatible: OpenAiCompatibleOnboarding,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OpenAiCompatibleOnboarding {
    /// Origins the operator allows over plain HTTP or at private addresses,
    /// offered as suggestions.
    pub private_endpoints: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AwsIdentity {
    pub principal_arn: String,
    pub external_id: String,
    pub role_name: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AzureIdentity {
    pub client_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GoogleIdentity {
    pub service_account: String,
}

/// One cloud's instructions: links and copyable scripts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AwsInstructions {
    /// What the Bedrock credential names (`role_arn`).
    pub role_arn: String,
    pub external_id: String,
    /// CloudFormation quick-create, with the parameters filled in; absent
    /// without a published template, and in GovCloud and China, whose
    /// consoles cannot read it.
    pub launch_url: Option<String>,
    pub template_url: Option<String>,
    /// Whether the role may also accept models' agreements.
    pub accept_model_agreements: bool,
    /// The staging bucket and batch service role the role may run batch
    /// inference jobs with; absent unless asked for.
    pub batch_bucket: Option<String>,
    pub batch_service_role_arn: Option<String>,
    /// The IAM actions the role is granted.
    pub actions: Vec<&'static str>,
    pub cli: String,
    pub terraform: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AzureInstructions {
    /// Admin consent to the gateway's app; returns with the tenant ID.
    pub consent_url: String,
    pub client_id: String,
    /// The role to grant, by resource kind.
    pub role: &'static str,
    /// Whether the scripts also create and grant the batch role.
    pub batch: bool,
    pub cli: String,
    pub terraform: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GoogleInstructions {
    /// What the Gemini credential names (`service_account`).
    pub service_account: String,
    pub project_id: String,
    /// The staging bucket the scripts grant access to; absent unless asked
    /// for.
    pub batch_bucket: Option<String>,
    pub gcloud: String,
    /// Opens Cloud Shell in the project; the script is pasted there.
    pub cloud_shell_url: String,
    pub terraform: String,
}

impl OnboardingIdentities {
    pub fn summary(&self, organization_id: OrganizationId) -> OnboardingSummary {
        OnboardingSummary {
            aws: self
                .aws_principal_arn
                .clone()
                .map(|principal_arn| AwsIdentity {
                    principal_arn,
                    external_id: super::credentials::aws_external_id(organization_id),
                    role_name: AWS_ROLE_NAME,
                }),
            azure: self
                .azure_client_id
                .clone()
                .map(|client_id| AzureIdentity { client_id }),
            google: self
                .google_service_account
                .clone()
                .map(|service_account| GoogleIdentity { service_account }),
            openai_compatible: OpenAiCompatibleOnboarding {
                private_endpoints: self.openai_compatible_private_endpoints.clone(),
            },
        }
    }

    /// `accept_agreements`: also let the role accept models' AWS
    /// Marketplace agreements (D-035), off unless asked for. `batch`: also
    /// let it run batch inference jobs through that bucket and service role.
    pub fn aws(
        &self,
        organization_id: OrganizationId,
        account_id: &str,
        region: &str,
        accept_agreements: bool,
        batch: Option<AwsBatch<'_>>,
    ) -> Result<AwsInstructions, OnboardingError> {
        let principal = self
            .aws_principal_arn
            .as_deref()
            .ok_or(OnboardingError::Unavailable)?;
        if account_id.len() != 12 || !account_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(OnboardingError::AwsAccount);
        }
        if !aws_region(region) {
            return Err(OnboardingError::AwsRegion);
        }
        let partition = aws_partition(region);
        if let Some(batch) = batch {
            let role_prefix = format!("arn:{partition}:iam::{account_id}:role/");
            if !s3_bucket(batch.bucket)
                || !batch
                    .service_role_arn
                    .strip_prefix(&role_prefix)
                    .is_some_and(iam_role_path)
            {
                return Err(OnboardingError::AwsBatch);
            }
        }
        let external_id = super::credentials::aws_external_id(organization_id);
        let role_arn = format!("arn:{partition}:iam::{account_id}:role/{AWS_ROLE_NAME}");
        // Quick-create reads the template from S3 in the console's own
        // partition; the published one is in the commercial partition, so
        // GovCloud and China tenants use the CLI or Terraform.
        let launch_url = self
            .aws_template_url
            .as_ref()
            .filter(|_| partition == "aws")
            .map(|template| {
                let mut url = Url::parse(&format!(
                    "https://{region}.console.aws.amazon.com/cloudformation/home"
                ))
                .expect("a valid console URL");
                url.query_pairs_mut().append_pair("region", region);
                // The console reads the quick-create form from the fragment.
                let mut query = url::form_urlencoded::Serializer::new(String::new());
                query
                    .append_pair("templateURL", template.as_str())
                    .append_pair("stackName", "qucelerate-gateway")
                    .append_pair("param_GatewayPrincipalArn", principal)
                    .append_pair("param_ExternalId", &external_id);
                // The template's parameter defaults to false, so a template
                // published before it existed still launches.
                if accept_agreements {
                    query.append_pair("param_AcceptModelAgreements", "true");
                }
                // Empty by default: no batch statements.
                if let Some(batch) = batch {
                    query
                        .append_pair("param_BatchBucket", batch.bucket)
                        .append_pair("param_BatchServiceRoleArn", batch.service_role_arn);
                }
                let query = query.finish();
                format!("{url}#/stacks/quickcreate?{query}")
            });
        let trust = serde_json::to_string_pretty(&serde_json::json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Principal": { "AWS": principal },
                "Action": "sts:AssumeRole",
                "Condition": { "StringEquals": { "sts:ExternalId": external_id } },
            }],
        }))
        .expect("serializable");
        let batch_statements = batch
            .map(|batch| bedrock_batch_statements(partition, region, account_id, batch))
            .unwrap_or_default();
        let permissions =
            serde_json::to_string_pretty(&bedrock_policy(accept_agreements, &batch_statements))
                .expect("serializable");
        let cli = format!(
            "# Run with credentials for AWS account {account_id}.\n\
             cat > qucelerate-trust.json <<'EOF'\n{trust}\nEOF\n\
             cat > qucelerate-bedrock.json <<'EOF'\n{permissions}\nEOF\n\
             aws iam create-role --role-name {AWS_ROLE_NAME} --max-session-duration 3600 \\\n  \
             --description 'The qucelerate gateway calls Amazon Bedrock for one organization.' \\\n  \
             --assume-role-policy-document file://qucelerate-trust.json\n\
             aws iam put-role-policy --role-name {AWS_ROLE_NAME} --policy-name bedrock \\\n  \
             --policy-document file://qucelerate-bedrock.json\n"
        );
        let terraform = format!(
            "# Apply with credentials for AWS account {account_id}.\n\
             resource \"aws_iam_role\" \"qucelerate_gateway\" {{\n  \
               name                 = \"{AWS_ROLE_NAME}\"\n  \
               description          = \"The qucelerate gateway calls Amazon Bedrock for one organization.\"\n  \
               max_session_duration = 3600\n  \
               assume_role_policy = jsonencode({{\n    \
                 Version = \"2012-10-17\"\n    \
                 Statement = [{{\n      \
                   Effect    = \"Allow\"\n      \
                   Principal = {{ AWS = \"{principal}\" }}\n      \
                   Action    = \"sts:AssumeRole\"\n      \
                   Condition = {{ StringEquals = {{ \"sts:ExternalId\" = \"{external_id}\" }} }}\n    \
                 }}]\n  \
               }})\n\
             }}\n\n\
             resource \"aws_iam_role_policy\" \"qucelerate_gateway_bedrock\" {{\n  \
               name = \"bedrock\"\n  \
               role = aws_iam_role.qucelerate_gateway.id\n  \
               policy = jsonencode({{\n    \
                 Version = \"2012-10-17\"\n    \
                 Statement = [{{\n      \
                   Effect = \"Allow\"\n      \
                   Action = [\n        {actions}\n      ]\n      \
                   Resource = \"*\"\n    \
                 }}{agreements}{batch}]\n  \
               }})\n\
             }}\n\n\
             output \"role_arn\" {{\n  value = aws_iam_role.qucelerate_gateway.arn\n}}\n",
            actions = terraform_actions(&BEDROCK_ACTIONS),
            agreements = if accept_agreements {
                format!(
                    ", {{\n      \
                     # Accept models' AWS Marketplace agreements (optional).\n      \
                     Sid    = \"AcceptModelAgreements\"\n      \
                     Effect = \"Allow\"\n      \
                     Action = [\n        {}\n      ]\n      \
                     Resource = \"*\"\n    \
                     }}",
                    terraform_actions(&BEDROCK_AGREEMENT_ACTIONS)
                )
            } else {
                String::new()
            },
            batch = batch_statements
                .iter()
                .map(|statement| format!(", {}", hcl_value(statement, 2)))
                .collect::<String>(),
        );
        let mut actions = BEDROCK_ACTIONS.to_vec();
        if accept_agreements {
            actions.extend(BEDROCK_AGREEMENT_ACTIONS);
        }
        if batch.is_some() {
            actions.extend(BEDROCK_BATCH_ACTIONS);
        }
        Ok(AwsInstructions {
            role_arn,
            external_id,
            launch_url,
            template_url: self.aws_template_url.as_ref().map(Url::to_string),
            accept_model_agreements: accept_agreements,
            batch_bucket: batch.map(|batch| batch.bucket.to_owned()),
            batch_service_role_arn: batch.map(|batch| batch.service_role_arn.to_owned()),
            actions,
            cli,
            terraform,
        })
    }

    /// `resource_id`: the tenant's Azure OpenAI or Foundry account, which the
    /// role is granted on; without it, the commands leave a placeholder.
    /// `foundry`: the resource is Azure AI Foundry, which needs the general
    /// Cognitive Services role rather than the OpenAI one. `batch`: also
    /// create a custom role for batch files and jobs and grant it on the
    /// resource (Azure OpenAI only).
    pub fn azure(
        &self,
        organization_id: OrganizationId,
        resource_id: Option<&str>,
        foundry: bool,
        batch: bool,
    ) -> Result<AzureInstructions, OnboardingError> {
        let client_id = self
            .azure_client_id
            .as_deref()
            .ok_or(OnboardingError::Unavailable)?;
        let redirect = self
            .azure_consent_redirect
            .as_ref()
            .ok_or(OnboardingError::Unavailable)?;
        if let Some(resource) = resource_id {
            if !azure_resource_id(resource) {
                return Err(OnboardingError::AzureResource);
            }
        }
        if batch && foundry {
            return Err(OnboardingError::AzureBatch);
        }
        let mut consent =
            Url::parse("https://login.microsoftonline.com/organizations/v2.0/adminconsent")
                .expect("a valid consent URL");
        consent
            .query_pairs_mut()
            .append_pair("client_id", client_id)
            .append_pair("scope", "https://cognitiveservices.azure.com/.default")
            .append_pair("redirect_uri", redirect.as_str())
            // Returned with the tenant: which organization is connecting.
            .append_pair("state", &organization_id.0.to_string());
        // Foundry accounts need the general role; OpenAI accounts the
        // narrower one.
        let role = if foundry {
            "Cognitive Services User"
        } else {
            "Cognitive Services OpenAI User"
        };
        let scope = resource_id.unwrap_or(
            "/subscriptions/<subscription>/resourceGroups/<group>/providers/Microsoft.CognitiveServices/accounts/<account>",
        );
        // A custom role is assignable at a subscription, not at one account.
        let subscription = resource_id
            .and_then(|resource| resource.split('/').nth(2))
            .map_or_else(
                || "/subscriptions/<subscription>".to_owned(),
                |id| format!("/subscriptions/{id}"),
            );
        // Custom role names are unique in the whole directory, and this one
        // is assignable in one subscription, so each subscription gets its
        // own: a second subscription in the directory can be set up too.
        let role_name = format!(
            "{AZURE_BATCH_ROLE} {}",
            subscription.trim_start_matches("/subscriptions/")
        );
        let batch_cli = if batch {
            let definition = serde_json::to_string_pretty(&serde_json::json!({
                "Name": role_name,
                "IsCustom": true,
                "Description": AZURE_BATCH_ROLE_DESCRIPTION,
                "Actions": [],
                "NotActions": [],
                "DataActions": AZURE_BATCH_DATA_ACTIONS,
                "NotDataActions": [],
                "AssignableScopes": [subscription],
            }))
            .expect("serializable");
            format!(
                "# Lets the app upload batch files and create and cancel batch jobs (optional).\n\
                 cat > qucelerate-batches.json <<'EOF'\n{definition}\nEOF\n\
                 az role definition list --name \"{role_name}\" --scope \"{subscription}\" \\\n  \
                 --query \"[0].id\" -o tsv | grep -q . \\\n  \
                 || az role definition create --role-definition @qucelerate-batches.json\n\
                 # A new role can take a few minutes to be assignable; run this again if it fails.\n\
                 az role assignment create --assignee {client_id} \\\n  \
                 --role \"{role_name}\" \\\n  \
                 --scope \"{scope}\"\n"
            )
        } else {
            String::new()
        };
        let batch_terraform = if batch {
            format!(
                "\n# Lets the app upload batch files and create and cancel batch jobs (optional).\n\
                 resource \"azurerm_role_definition\" \"qucelerate_gateway_batches\" {{\n  \
                   name        = \"{role_name}\"\n  \
                   scope       = \"{subscription}\"\n  \
                   description = \"{AZURE_BATCH_ROLE_DESCRIPTION}\"\n  \
                   permissions {{\n    \
                     data_actions = [\n      {actions}\n    ]\n  \
                   }}\n  \
                   assignable_scopes = [\"{subscription}\"]\n\
                 }}\n\n\
                 resource \"azurerm_role_assignment\" \"qucelerate_gateway_batches\" {{\n  \
                   scope              = \"{scope}\"\n  \
                   role_definition_id = azurerm_role_definition.qucelerate_gateway_batches.role_definition_resource_id\n  \
                   principal_id       = azuread_service_principal.qucelerate_gateway.object_id\n\
                 }}\n",
                actions = AZURE_BATCH_DATA_ACTIONS
                    .iter()
                    .map(|action| format!("\"{action}\""))
                    .collect::<Vec<_>>()
                    .join(",\n      "),
            )
        } else {
            String::new()
        };
        let cli = format!(
            "# Run as an administrator of the directory, after consenting.\n\
             az ad sp show --id {client_id} --query id -o tsv >/dev/null \\\n  \
             || az ad sp create --id {client_id}\n\
             az role assignment create --assignee {client_id} \\\n  \
             --role \"{role}\" \\\n  \
             --scope \"{scope}\"\n\
             # Lets discovery list the resource's deployments (read only).\n\
             az role assignment create --assignee {client_id} \\\n  \
             --role \"Reader\" \\\n  \
             --scope \"{scope}\"\n\
             {batch_cli}"
        );
        let terraform = format!(
            "# Apply as an administrator of the directory.\n\
             resource \"azuread_service_principal\" \"qucelerate_gateway\" {{\n  \
               client_id    = \"{client_id}\"\n  \
               use_existing = true\n\
             }}\n\n\
             resource \"azurerm_role_assignment\" \"qucelerate_gateway\" {{\n  \
               scope                = \"{scope}\"\n  \
               role_definition_name = \"{role}\"\n  \
               principal_id         = azuread_service_principal.qucelerate_gateway.object_id\n\
             }}\n\n\
             # Lets discovery list the resource's deployments (read only).\n\
             resource \"azurerm_role_assignment\" \"qucelerate_gateway_reader\" {{\n  \
               scope                = \"{scope}\"\n  \
               role_definition_name = \"Reader\"\n  \
               principal_id         = azuread_service_principal.qucelerate_gateway.object_id\n\
             }}\n\
             {batch_terraform}"
        );
        Ok(AzureInstructions {
            consent_url: consent.to_string(),
            client_id: client_id.to_owned(),
            role,
            batch,
            cli,
            terraform,
        })
    }

    /// `batch_bucket`: also let the service account and the project's
    /// Vertex AI service agent read and write that staging bucket.
    pub fn google(
        &self,
        project_id: &str,
        batch_bucket: Option<&str>,
    ) -> Result<GoogleInstructions, OnboardingError> {
        let gateway = self
            .google_service_account
            .as_deref()
            .ok_or(OnboardingError::Unavailable)?;
        if !google_project(project_id) {
            return Err(OnboardingError::GoogleProject);
        }
        if batch_bucket.is_some_and(|bucket| !gcs_bucket(bucket)) {
            return Err(OnboardingError::GoogleBatch);
        }
        let account = format!("{GOOGLE_ACCOUNT_ID}@{project_id}.iam.gserviceaccount.com");
        let gcloud = format!(
            "# Run in project {project_id}, as an owner of it. Safe to run again.\n\
             gcloud services enable aiplatform.googleapis.com iamcredentials.googleapis.com \\\n  \
             --project {project_id}\n\
             gcloud iam service-accounts describe {account} --project {project_id} >/dev/null 2>&1 \\\n  \
             || gcloud iam service-accounts create {GOOGLE_ACCOUNT_ID} --project {project_id} \\\n  \
             --display-name \"Qucelerate gateway\"\n\
             # A new service account can take a minute to be usable in a policy.\n\
             granted=\n\
             for attempt in 1 2 3 4 5 6 7 8 9 10 11 12; do\n  \
             gcloud projects add-iam-policy-binding {project_id} \\\n    \
             --member serviceAccount:{account} --role roles/aiplatform.user --condition None \\\n    \
             && {{ granted=yes; break; }}\n  \
             sleep 10\n\
             done\n\
             if [ -z \"$granted\" ]; then\n  \
             echo \"roles/aiplatform.user could not be granted yet; run this script again.\" >&2\n  \
             false\n\
             else\n  \
             gcloud iam service-accounts add-iam-policy-binding {account} --project {project_id} \\\n    \
             --member serviceAccount:{gateway} --role roles/iam.serviceAccountTokenCreator\n\
             {batch_gcloud}\
             fi\n",
            batch_gcloud = batch_bucket.map_or_else(String::new, |bucket| format!(
                "  # Batches (optional): the service account stages them in gs://{bucket},\n  \
                 # and Agent Platform's service agent reads and writes them.\n  \
                 gcloud beta services identity create --service aiplatform.googleapis.com \\\n    \
                 --project {project_id} >/dev/null\n  \
                 number=$(gcloud projects describe {project_id} --format 'value(projectNumber)')\n  \
                 for member in serviceAccount:{account} \\\n    \
                 \"serviceAccount:service-$number@gcp-sa-aiplatform.iam.gserviceaccount.com\"; do\n    \
                 for role in roles/storage.objectCreator roles/storage.objectViewer; do\n      \
                 gcloud storage buckets add-iam-policy-binding gs://{bucket} \\\n        \
                 --member \"$member\" --role \"$role\" >/dev/null\n    \
                 done\n  \
                 done\n"
            )),
        );
        let mut cloud_shell =
            Url::parse("https://shell.cloud.google.com/").expect("a valid Cloud Shell URL");
        cloud_shell
            .query_pairs_mut()
            .append_pair("show", "terminal")
            .append_pair("cloudshell_project", project_id);
        let terraform = format!(
            "# Apply as an owner of project {project_id}.\n\
             resource \"google_project_service\" \"qucelerate_gateway\" {{\n  \
               for_each           = toset([\"aiplatform.googleapis.com\", \"iamcredentials.googleapis.com\"])\n  \
               project            = \"{project_id}\"\n  \
               service            = each.value\n  \
               disable_on_destroy = false\n\
             }}\n\n\
             resource \"google_service_account\" \"qucelerate_gateway\" {{\n  \
               project      = \"{project_id}\"\n  \
               account_id   = \"{GOOGLE_ACCOUNT_ID}\"\n  \
               display_name = \"Qucelerate gateway\"\n\
             }}\n\n\
             resource \"google_project_iam_member\" \"qucelerate_gateway_agent_platform\" {{\n  \
               project = \"{project_id}\"\n  \
               role    = \"roles/aiplatform.user\"\n  \
               member  = \"serviceAccount:${{google_service_account.qucelerate_gateway.email}}\"\n\
             }}\n\n\
             resource \"google_service_account_iam_member\" \"qucelerate_gateway_impersonation\" {{\n  \
               service_account_id = google_service_account.qucelerate_gateway.name\n  \
               role               = \"roles/iam.serviceAccountTokenCreator\"\n  \
               member             = \"serviceAccount:{gateway}\"\n\
             }}\n\
             {batch_terraform}",
            batch_terraform = batch_bucket.map_or_else(String::new, |bucket| format!(
                "\n# Batches (optional): the service account stages them in gs://{bucket},\n\
                 # and Agent Platform's service agent reads and writes them.\n\
                 resource \"google_project_service_identity\" \"qucelerate_gateway_agent_platform\" {{\n  \
                   provider = google-beta\n  \
                   project  = \"{project_id}\"\n  \
                   service  = \"aiplatform.googleapis.com\"\n\
                 }}\n\n\
                 resource \"google_storage_bucket_iam_member\" \"qucelerate_gateway_batches\" {{\n  \
                   for_each = toset([\"roles/storage.objectCreator\", \"roles/storage.objectViewer\"])\n  \
                   bucket   = \"{bucket}\"\n  \
                   role     = each.value\n  \
                   member   = \"serviceAccount:${{google_service_account.qucelerate_gateway.email}}\"\n\
                 }}\n\n\
                 resource \"google_storage_bucket_iam_member\" \"qucelerate_gateway_agent_platform_batches\" {{\n  \
                   for_each = toset([\"roles/storage.objectCreator\", \"roles/storage.objectViewer\"])\n  \
                   bucket   = \"{bucket}\"\n  \
                   role     = each.value\n  \
                   member   = \"serviceAccount:${{google_project_service_identity.qucelerate_gateway_agent_platform.email}}\"\n\
                 }}\n"
            )),
        );
        Ok(GoogleInstructions {
            service_account: account,
            project_id: project_id.to_owned(),
            batch_bucket: batch_bucket.map(str::to_owned),
            gcloud,
            cloud_shell_url: cloud_shell.to_string(),
            terraform,
        })
    }
}

/// What signing in with a credential, as inference would, found.
#[derive(Debug)]
pub enum Probe {
    /// Signed in and listed the models it can call; `region` is where, for
    /// Bedrock.
    Listed {
        models: Vec<super::discovery::DiscoveredModel>,
        region: Option<String>,
    },
    /// The gateway has no configuration that can use this kind of
    /// credential for the family (for example Bedrock without a region).
    NotConfigured,
    /// A tenant's OpenAI-compatible endpoint the egress policy refuses: plain
    /// HTTP, or private addresses, at an origin the operator has not allowed.
    /// Nothing was sent to it.
    EndpointNotAllowed,
    /// A tenant's OpenAI-compatible endpoint whose host does not resolve.
    EndpointUnresolved,
}

/// Signs in with a tenant's credential the way inference would for the
/// organization and lists models, storing and generating nothing. Callers
/// check cloud-principal approval first.
#[async_trait::async_trait]
pub trait CredentialProbe: Send + Sync {
    /// `adapter_instance_key`: the provider account the credential is for,
    /// which chooses its configuration exactly as inference does.
    async fn probe(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: super::catalog::ProviderFamily,
        secret: &super::credentials::ProviderSecret,
    ) -> Result<Probe, super::inference::ProviderError>;
}

pub const BEDROCK_ACTIONS: [&str; 10] = [
    "bedrock:InvokeModel",
    "bedrock:InvokeModelWithResponseStream",
    "bedrock:ListFoundationModels",
    "bedrock:ListInferenceProfiles",
    // Read-only and optional: what discovery shows about each model (D-018).
    "bedrock:GetFoundationModelAvailability",
    // Read-only and optional: the account's own resources discovery lists
    // after the shared catalog (provider::bedrock, `OWN_RESOURCES`).
    "bedrock:ListProvisionedModelThroughputs",
    "bedrock:ListImportedModels",
    "bedrock:ListMarketplaceModelEndpoints",
    "bedrock:ListCustomModelDeployments",
    "bedrock:GetCustomModelDeployment",
];

/// What the gateway's Bedrock batch jobs take (D-021; provider::bedrock::batch
/// and docs/operations.md, *Batch staging*), granted only when the tenant
/// names a staging bucket and a batch service role. The Bedrock user guide's
/// "Required permissions for batch inference" lists the job actions;
/// creating a job with tags also takes `bedrock:TagResource`, and passing
/// its service role `iam:PassRole` (Service Authorization Reference, Amazon
/// Bedrock, `CreateModelInvocationJob`). The gateway writes the input to and
/// reads the output from the bucket itself.
pub const BEDROCK_BATCH_ACTIONS: [&str; 9] = [
    "bedrock:CreateModelInvocationJob",
    "bedrock:ListModelInvocationJobs",
    "bedrock:GetModelInvocationJob",
    "bedrock:StopModelInvocationJob",
    "bedrock:TagResource",
    "iam:PassRole",
    "s3:PutObject",
    "s3:GetObject",
    "s3:ListBucket",
];

/// The custom role the Azure batch section creates: `Cognitive Services
/// OpenAI User` reads files and batches but cannot upload a file or create
/// a batch (Azure built-in roles, AI + machine learning). These are the
/// documented data actions for both (Azure permissions, AI + machine
/// learning: `files/write`, `batch-jobs/write`, `batches/action` and
/// `batches/cancel/action`).
pub const AZURE_BATCH_DATA_ACTIONS: [&str; 4] = [
    "Microsoft.CognitiveServices/accounts/OpenAI/files/write",
    "Microsoft.CognitiveServices/accounts/OpenAI/batch-jobs/write",
    "Microsoft.CognitiveServices/accounts/OpenAI/batches/action",
    "Microsoft.CognitiveServices/accounts/OpenAI/batches/cancel/action",
];
pub const AZURE_BATCH_ROLE: &str = "Qucelerate gateway batches";
const AZURE_BATCH_ROLE_DESCRIPTION: &str =
    "Lets the qucelerate gateway upload batch files and create and cancel batch jobs.";

/// What accepting a model's AWS Marketplace agreement takes (D-035), granted
/// only when the tenant opts in. Bedrock's `ListFoundationModelAgreementOffers`
/// and `CreateFoundationModelAgreement` read and accept an offer (Service
/// Authorization Reference, Amazon Bedrock; neither takes a resource, so
/// `*`). The agreement is an AWS Marketplace subscription: the Bedrock user
/// guide's model access page says an identity that requests access to a
/// model with a product ID "must have a policy attached that allows
/// `aws-marketplace:Subscribe`", and lists `ViewSubscriptions` (read-only)
/// with it. `aws-marketplace:Unsubscribe` is left out: the page names it
/// only among the prerequisites for automatic subscription on a first call,
/// and the gateway never unsubscribes.
pub const BEDROCK_AGREEMENT_ACTIONS: [&str; 4] = [
    "bedrock:ListFoundationModelAgreementOffers",
    "bedrock:CreateFoundationModelAgreement",
    "aws-marketplace:Subscribe",
    "aws-marketplace:ViewSubscriptions",
];

fn bedrock_policy(
    accept_agreements: bool,
    batch_statements: &[serde_json::Value],
) -> serde_json::Value {
    let mut statements = vec![serde_json::json!(
        { "Effect": "Allow", "Action": BEDROCK_ACTIONS, "Resource": "*" }
    )];
    if accept_agreements {
        statements.push(serde_json::json!({
            "Sid": "AcceptModelAgreements",
            "Effect": "Allow",
            "Action": BEDROCK_AGREEMENT_ACTIONS,
            "Resource": "*",
        }));
    }
    statements.extend_from_slice(batch_statements);
    serde_json::json!({ "Version": "2012-10-17", "Statement": statements })
}

/// The batch statements of [`BEDROCK_BATCH_ACTIONS`], scoped where the
/// action allows: jobs in the region and account, the one service role
/// (passed only to Bedrock) and the one bucket.
fn bedrock_batch_statements(
    partition: &str,
    region: &str,
    account_id: &str,
    batch: AwsBatch<'_>,
) -> Vec<serde_json::Value> {
    let [create, list, get, stop, tag, pass, put, read, list_bucket] = BEDROCK_BATCH_ACTIONS;
    let bucket = batch.bucket;
    vec![
        // A job is created on the model's resource and the job's own, and
        // listing takes no resource.
        serde_json::json!({
            "Sid": "BatchJobs",
            "Effect": "Allow",
            "Action": [create, list],
            "Resource": "*",
        }),
        serde_json::json!({
            "Sid": "BatchJobControl",
            "Effect": "Allow",
            "Action": [get, stop, tag],
            "Resource": format!("arn:{partition}:bedrock:{region}:{account_id}:model-invocation-job/*"),
        }),
        serde_json::json!({
            "Sid": "BatchServiceRole",
            "Effect": "Allow",
            "Action": pass,
            "Resource": batch.service_role_arn,
            "Condition": { "StringEquals": { "iam:PassedToService": "bedrock.amazonaws.com" } },
        }),
        serde_json::json!({
            "Sid": "BatchStagingObjects",
            "Effect": "Allow",
            "Action": [put, read],
            "Resource": format!("arn:{partition}:s3:::{bucket}/*"),
        }),
        serde_json::json!({
            "Sid": "BatchStagingBucket",
            "Effect": "Allow",
            "Action": list_bucket,
            "Resource": format!("arn:{partition}:s3:::{bucket}"),
        }),
    ]
}

/// A JSON value as an HCL expression, for `jsonencode(...)`: objects open
/// at `indent` levels of two spaces. Strings here are validated input with
/// no `${` or `%{`, so JSON's escaping is HCL's.
fn hcl_value(value: &serde_json::Value, indent: usize) -> String {
    match value {
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|item| hcl_value(item, indent))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        serde_json::Value::Object(fields) => {
            let pad = "  ".repeat(indent + 1);
            let body = fields
                .iter()
                .map(|(key, value)| {
                    let key = if key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                        key.clone()
                    } else {
                        serde_json::Value::from(key.as_str()).to_string()
                    };
                    format!("{pad}{key} = {}", hcl_value(value, indent + 1))
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("{{\n{body}\n{}}}", "  ".repeat(indent))
        }
        other => other.to_string(),
    }
}

/// An S3 general purpose bucket name: 3 to 63 lowercase letters, digits,
/// dots and hyphens, starting and ending with a letter or digit.
fn s3_bucket(value: &str) -> bool {
    let bytes = value.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
        && !value.contains("..")
}

/// A Cloud Storage bucket name: 3 to 63 lowercase letters, digits,
/// hyphens, underscores and dots, starting and ending with a letter or
/// digit (dotted names may be longer, up to 222).
fn gcs_bucket(value: &str) -> bool {
    let bytes = value.as_bytes();
    let limit = if value.contains('.') { 222 } else { 63 };
    (3..=limit).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
        })
        && !value.contains("..")
}

/// An IAM role's path and name after `:role/`: `name` or `path/name`, of
/// the characters IAM allows.
fn iam_role_path(value: &str) -> bool {
    let name = value.rsplit('/').next().unwrap_or_default();
    !name.is_empty()
        && name.len() <= 64
        && value.len() <= 512 + 64
        && !value.starts_with('/')
        && !value.contains("//")
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'+' | b'=' | b',' | b'.' | b'@' | b'_' | b'-' | b'/')
        })
}

fn terraform_actions(actions: &[&str]) -> String {
    actions
        .iter()
        .map(|action| format!("\"{action}\""))
        .collect::<Vec<_>>()
        .join(",\n        ")
}

/// `us-east-1`, `eu-central-2`, `us-gov-west-1`, `cn-north-1`, ...
fn aws_region(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    (3..=4).contains(&parts.len())
        && parts[..parts.len() - 1]
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase()))
        && parts[parts.len() - 1].bytes().all(|b| b.is_ascii_digit())
        && !parts[parts.len() - 1].is_empty()
}

fn aws_partition(region: &str) -> &'static str {
    if region.starts_with("us-gov-") {
        "aws-us-gov"
    } else if region.starts_with("cn-") {
        "aws-cn"
    } else {
        "aws"
    }
}

/// `/subscriptions/<uuid>/resourceGroups/<group>/providers/Microsoft.CognitiveServices/accounts/<name>`
pub(crate) fn azure_resource_id(value: &str) -> bool {
    let parts: Vec<&str> = value.split('/').collect();
    let safe = |part: &str| {
        !part.is_empty()
            && part.len() <= 90
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '(' | ')'))
    };
    parts.len() == 9
        && parts[0].is_empty()
        && parts[1].eq_ignore_ascii_case("subscriptions")
        && uuid::Uuid::parse_str(parts[2]).is_ok()
        && parts[3].eq_ignore_ascii_case("resourceGroups")
        && safe(parts[4])
        && parts[5].eq_ignore_ascii_case("providers")
        && parts[6].eq_ignore_ascii_case("Microsoft.CognitiveServices")
        && parts[7].eq_ignore_ascii_case("accounts")
        && safe(parts[8])
}

/// 6 to 30 characters: lowercase letters, digits and hyphens, starting with
/// a letter and not ending with a hyphen.
fn google_project(value: &str) -> bool {
    let bytes = value.as_bytes();
    (6..=30).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script is valid shell, with no warnings (`bash -n` only parses it).
    fn shell_parses(script: &str) {
        use std::io::Write;
        let mut child = std::process::Command::new("bash")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("bash is available");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        // bash only warns about an unterminated here-document.
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "{}\n{script}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The Terraform is valid HCL with only resource and output blocks.
    fn terraform_parses(terraform: &str) {
        let body: hcl::Body =
            hcl::from_str(terraform).unwrap_or_else(|error| panic!("{error}\n{terraform}"));
        assert!(body.blocks().count() > 0);
        assert!(
            body.blocks()
                .all(|block| matches!(block.identifier(), "resource" | "output")),
            "{terraform}"
        );
    }

    #[test]
    fn the_syntax_checks_reject_broken_scripts() {
        for broken in [
            "cat <<'EOF'\nno end",
            "if true; then echo",
            "echo \"unclosed",
        ] {
            assert!(
                std::panic::catch_unwind(|| shell_parses(broken)).is_err(),
                "{broken}"
            );
        }
        for broken in [
            "resource \"aws_iam_role\" \"x\" {\n  name = \"a\"\n",
            "resource \"aws_iam_role\" \"x\" { name = }",
            "provider \"aws\" {}\n",
        ] {
            assert!(
                std::panic::catch_unwind(|| terraform_parses(broken)).is_err(),
                "{broken}"
            );
        }
    }

    fn identities() -> OnboardingIdentities {
        OnboardingIdentities {
            openai_compatible_private_endpoints: Vec::new(),
            aws_principal_arn: Some("arn:aws:iam::123456789012:user/example-gateway".into()),
            aws_template_url: Some(
                Url::parse(
                    "https://bucket.s3.us-east-2.amazonaws.com/templates/aws-bedrock-role-v1.yaml",
                )
                .unwrap(),
            ),
            azure_client_id: Some("00000000-0000-4000-8000-000000000020".into()),
            azure_consent_redirect: Some(
                Url::parse("https://gateway.example/api/admin/v1/onboarding/azure/consent")
                    .unwrap(),
            ),
            google_service_account: Some("gateway-dev@gw-project.iam.gserviceaccount.com".into()),
        }
    }

    #[test]
    fn only_configured_clouds_are_offered() {
        let org = OrganizationId::new();
        let summary = OnboardingIdentities::default().summary(org);
        assert_eq!(
            (summary.aws, summary.azure, summary.google),
            (None, None, None)
        );
        let summary = identities().summary(org);
        assert_eq!(
            summary.aws.unwrap().external_id,
            format!("qucelerate-{}", org.0)
        );
        assert!(matches!(
            OnboardingIdentities::default().aws(org, "123456789012", "us-east-1", false, None),
            Err(OnboardingError::Unavailable)
        ));
    }

    #[test]
    fn aws_instructions_name_the_gateway_and_the_external_id() {
        let org = OrganizationId::new();
        let external_id = format!("qucelerate-{}", org.0);
        let aws = identities()
            .aws(org, "123456789012", "us-west-2", false, None)
            .unwrap();
        assert_eq!(
            aws.role_arn,
            "arn:aws:iam::123456789012:role/QucelerateGateway"
        );
        let launch = aws.launch_url.unwrap();
        assert!(launch.starts_with(
            "https://us-west-2.console.aws.amazon.com/cloudformation/home?region=us-west-2#/stacks/quickcreate?"
        ));
        let fragment = launch.split_once("quickcreate?").unwrap().1;
        let params: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(fragment.as_bytes())
                .into_owned()
                .collect();
        assert_eq!(params["param_ExternalId"], external_id);
        assert_eq!(
            params["param_GatewayPrincipalArn"],
            "arn:aws:iam::123456789012:user/example-gateway"
        );
        assert!(params["templateURL"].ends_with("aws-bedrock-role-v1.yaml"));
        shell_parses(&aws.cli);
        terraform_parses(&aws.terraform);
        for script in [&aws.cli, &aws.terraform] {
            assert!(script.contains(&external_id), "{script}");
            assert!(script.contains("arn:aws:iam::123456789012:user/example-gateway"));
            assert!(script.contains("bedrock:InvokeModelWithResponseStream"));
            assert!(script.contains("QucelerateGateway"));
        }
        // GovCloud and China roles are in their own partitions, and their
        // consoles cannot read the published template.
        for (region, partition) in [("us-gov-west-1", "aws-us-gov"), ("cn-north-1", "aws-cn")] {
            let other = identities()
                .aws(org, "123456789012", region, false, None)
                .unwrap();
            assert!(
                other.role_arn.starts_with(&format!("arn:{partition}:")),
                "{region}"
            );
            assert_eq!(other.launch_url, None, "{region}");
            shell_parses(&other.cli);
            terraform_parses(&other.terraform);
        }
        for (account, region) in [
            ("12345", "us-east-1"),
            ("123456789012", "US-EAST-1"),
            ("123456789012", "us-east"),
        ] {
            assert!(
                identities().aws(org, account, region, false, None).is_err(),
                "{account} {region}"
            );
        }
    }

    /// The agreement permissions are granted only when asked for (D-035).
    #[test]
    fn aws_roles_accept_agreements_only_when_opted_in() {
        let org = OrganizationId::new();
        let plain = identities()
            .aws(org, "123456789012", "us-east-2", false, None)
            .unwrap();
        assert!(!plain.accept_model_agreements);
        for action in BEDROCK_AGREEMENT_ACTIONS {
            assert!(!plain.cli.contains(action), "{action}");
            assert!(!plain.terraform.contains(action), "{action}");
            assert!(!plain.actions.contains(&action), "{action}");
        }
        assert!(
            !plain
                .launch_url
                .unwrap()
                .contains("param_AcceptModelAgreements")
        );

        let opted = identities()
            .aws(org, "123456789012", "us-east-2", true, None)
            .unwrap();
        assert!(opted.accept_model_agreements);
        shell_parses(&opted.cli);
        terraform_parses(&opted.terraform);
        for action in BEDROCK_AGREEMENT_ACTIONS {
            assert!(opted.cli.contains(action), "{action}");
            assert!(opted.terraform.contains(action), "{action}");
            assert!(opted.actions.contains(&action), "{action}");
        }
        // Unsubscribe is only an automatic-subscription prerequisite, and the
        // gateway never unsubscribes.
        assert!(!opted.cli.contains("aws-marketplace:Unsubscribe"));
        // The base actions are kept, in their own statement.
        assert!(opted.cli.contains("bedrock:InvokeModelWithResponseStream"));
        assert!(opted.cli.contains("AcceptModelAgreements"));
        assert!(
            opted
                .launch_url
                .unwrap()
                .contains("param_AcceptModelAgreements=true")
        );
        // The CLI's policy document is valid JSON with both statements.
        let policy = opted
            .cli
            .split("cat > qucelerate-bedrock.json <<'EOF'\n")
            .nth(1)
            .and_then(|rest| rest.split("\nEOF").next())
            .unwrap();
        let policy: serde_json::Value = serde_json::from_str(policy).unwrap();
        assert_eq!(policy["Statement"].as_array().unwrap().len(), 2);
        assert_eq!(
            policy["Statement"][1]["Action"],
            serde_json::json!(BEDROCK_AGREEMENT_ACTIONS)
        );
    }

    #[test]
    fn azure_instructions_consent_then_grant_the_app_a_role() {
        let org = OrganizationId::new();
        let resource = "/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg-ai/providers/Microsoft.CognitiveServices/accounts/acme-openai";
        let azure = identities()
            .azure(org, Some(resource), false, false)
            .unwrap();
        let consent = Url::parse(&azure.consent_url).unwrap();
        let params: std::collections::HashMap<String, String> =
            consent.query_pairs().into_owned().collect();
        assert_eq!(params["client_id"], "00000000-0000-4000-8000-000000000020");
        assert_eq!(params["state"], org.0.to_string());
        assert_eq!(
            params["redirect_uri"],
            "https://gateway.example/api/admin/v1/onboarding/azure/consent"
        );
        shell_parses(&azure.cli);
        terraform_parses(&azure.terraform);
        for script in [&azure.cli, &azure.terraform] {
            assert!(script.contains(resource), "{script}");
            assert!(script.contains("00000000-0000-4000-8000-000000000020"));
        }
        assert!(
            identities()
                .azure(org, None, false, false)
                .unwrap()
                .cli
                .contains("<account>")
        );
        // Each service's own role, in every script.
        assert_eq!(azure.role, "Cognitive Services OpenAI User");
        let foundry = identities()
            .azure(org, Some(resource), true, false)
            .unwrap();
        assert_eq!(foundry.role, "Cognitive Services User");
        for script in [&foundry.cli, &foundry.terraform] {
            assert!(script.contains("\"Cognitive Services User\""), "{script}");
            assert!(!script.contains("OpenAI User"), "{script}");
        }
        shell_parses(&foundry.cli);
        terraform_parses(&foundry.terraform);
        // And Reader on the same resource, so discovery can list its
        // deployments (D-018).
        for script in [
            &azure.cli,
            &azure.terraform,
            &foundry.cli,
            &foundry.terraform,
        ] {
            assert!(script.contains("\"Reader\""), "{script}");
        }
        assert!(matches!(
            identities().azure(org, Some("/subscriptions/x; rm -rf /"), false, false),
            Err(OnboardingError::AzureResource)
        ));
    }

    #[test]
    fn google_instructions_let_the_gateway_impersonate_a_new_account() {
        let google = identities().google("acme-ai-123", None).unwrap();
        assert_eq!(
            google.service_account,
            "qucelerate-gateway@acme-ai-123.iam.gserviceaccount.com"
        );
        shell_parses(&google.gcloud);
        terraform_parses(&google.terraform);
        for script in [&google.gcloud, &google.terraform] {
            assert!(
                script.contains("gateway-dev@gw-project.iam.gserviceaccount.com"),
                "{script}"
            );
            assert!(script.contains("roles/iam.serviceAccountTokenCreator"));
            assert!(script.contains("roles/aiplatform.user"));
        }
        // Rerunnable, and patient with a service account just created.
        assert!(google.gcloud.contains(
            "gcloud iam service-accounts describe qucelerate-gateway@acme-ai-123.iam.gserviceaccount.com"
        ));
        assert!(google.gcloud.contains("granted=yes; break;"));
        assert!(google.gcloud.ends_with("fi\n"));
        assert!(
            google
                .cloud_shell_url
                .contains("cloudshell_project=acme-ai-123")
        );
        for invalid in ["Acme", "ab", "acme-", "acme ai", "acme;rm"] {
            assert!(identities().google(invalid, None).is_err(), "{invalid}");
        }
    }

    /// Batch permissions are granted only when a bucket and service role are
    /// named, scoped to them (D-021).
    #[test]
    fn aws_roles_run_batches_only_when_given_a_bucket_and_role() {
        let org = OrganizationId::new();
        let plain = identities()
            .aws(org, "123456789012", "us-east-2", false, None)
            .unwrap();
        assert_eq!(
            (plain.batch_bucket, plain.batch_service_role_arn),
            (None, None)
        );
        for action in BEDROCK_BATCH_ACTIONS {
            assert!(!plain.cli.contains(action), "{action}");
            assert!(!plain.terraform.contains(action), "{action}");
            assert!(!plain.actions.contains(&action), "{action}");
        }
        assert!(!plain.launch_url.unwrap().contains("param_Batch"));
        // Discovery's optional read-only lists are always there.
        for action in [
            "bedrock:ListCustomModelDeployments",
            "bedrock:GetCustomModelDeployment",
            "bedrock:ListImportedModels",
        ] {
            assert!(plain.actions.contains(&action), "{action}");
            assert!(plain.terraform.contains(action), "{action}");
        }

        let role = "arn:aws:iam::123456789012:role/service-role/BedrockBatch";
        let batch = AwsBatch {
            bucket: "acme-gateway-batches",
            service_role_arn: role,
        };
        let opted = identities()
            .aws(org, "123456789012", "us-east-2", true, Some(batch))
            .unwrap();
        assert_eq!(opted.batch_bucket.as_deref(), Some("acme-gateway-batches"));
        assert_eq!(opted.batch_service_role_arn.as_deref(), Some(role));
        shell_parses(&opted.cli);
        terraform_parses(&opted.terraform);
        for action in BEDROCK_BATCH_ACTIONS {
            assert!(opted.cli.contains(action), "{action}");
            assert!(opted.terraform.contains(action), "{action}");
            assert!(opted.actions.contains(&action), "{action}");
        }
        let launch = opted.launch_url.unwrap();
        let fragment = launch.split_once("quickcreate?").unwrap().1;
        let params: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(fragment.as_bytes())
                .into_owned()
                .collect();
        assert_eq!(params["param_BatchBucket"], "acme-gateway-batches");
        assert_eq!(params["param_BatchServiceRoleArn"], role);
        let policy = opted
            .cli
            .split("cat > qucelerate-bedrock.json <<'EOF'\n")
            .nth(1)
            .and_then(|rest| rest.split("\nEOF").next())
            .unwrap();
        let policy: serde_json::Value = serde_json::from_str(policy).unwrap();
        let statements = policy["Statement"].as_array().unwrap();
        assert_eq!(statements.len(), 7);
        let by_sid = |sid: &str| {
            statements
                .iter()
                .find(|statement| statement["Sid"] == sid)
                .unwrap()
                .clone()
        };
        assert_eq!(
            by_sid("BatchJobControl")["Resource"],
            "arn:aws:bedrock:us-east-2:123456789012:model-invocation-job/*"
        );
        let pass = by_sid("BatchServiceRole");
        assert_eq!(pass["Resource"], role);
        assert_eq!(
            pass["Condition"]["StringEquals"]["iam:PassedToService"],
            "bedrock.amazonaws.com"
        );
        assert_eq!(
            by_sid("BatchStagingObjects")["Resource"],
            "arn:aws:s3:::acme-gateway-batches/*"
        );
        assert_eq!(
            by_sid("BatchStagingBucket")["Resource"],
            "arn:aws:s3:::acme-gateway-batches"
        );
        // The Terraform policy is the same document.
        let body: hcl::Body = hcl::from_str(&opted.terraform).unwrap();
        assert!(
            opted
                .terraform
                .contains("\"iam:PassedToService\" = \"bedrock.amazonaws.com\"")
        );
        assert!(body.blocks().count() >= 2);
        // GovCloud resources are in their own partition.
        let gov = identities()
            .aws(
                org,
                "123456789012",
                "us-gov-west-1",
                false,
                Some(AwsBatch {
                    bucket: "acme-batches",
                    service_role_arn: "arn:aws-us-gov:iam::123456789012:role/BedrockBatch",
                }),
            )
            .unwrap();
        assert!(gov.cli.contains("arn:aws-us-gov:s3:::acme-batches/*"));
        terraform_parses(&gov.terraform);
        for (bucket, service_role_arn) in [
            ("Acme", role),
            ("ab", role),
            ("acme..batches", role),
            ("acme\"; rm", role),
            ("acme", "arn:aws:iam::999999999999:role/BedrockBatch"),
            ("acme", "arn:aws-cn:iam::123456789012:role/BedrockBatch"),
            ("acme", "arn:aws:iam::123456789012:role/"),
            ("acme", "arn:aws:iam::123456789012:role/a b"),
            ("acme", "arn:aws:iam::123456789012:user/BedrockBatch"),
        ] {
            assert!(
                matches!(
                    identities().aws(
                        org,
                        "123456789012",
                        "us-east-2",
                        false,
                        Some(AwsBatch {
                            bucket,
                            service_role_arn
                        })
                    ),
                    Err(OnboardingError::AwsBatch)
                ),
                "{bucket} {service_role_arn}"
            );
        }
    }

    #[test]
    fn azure_batches_get_a_custom_role_on_the_resource() {
        let org = OrganizationId::new();
        let resource = "/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg-ai/providers/Microsoft.CognitiveServices/accounts/acme-openai";
        let plain = identities()
            .azure(org, Some(resource), false, false)
            .unwrap();
        assert!(!plain.batch);
        for script in [&plain.cli, &plain.terraform] {
            assert!(!script.contains(AZURE_BATCH_ROLE), "{script}");
        }
        let azure = identities()
            .azure(org, Some(resource), false, true)
            .unwrap();
        assert!(azure.batch);
        shell_parses(&azure.cli);
        terraform_parses(&azure.terraform);
        for script in [&azure.cli, &azure.terraform] {
            for action in AZURE_BATCH_DATA_ACTIONS {
                assert!(script.contains(action), "{action}");
            }
            assert!(script.contains(AZURE_BATCH_ROLE), "{script}");
            // Assignable at the subscription, granted on the resource.
            assert!(
                script.contains("\"/subscriptions/00000000-0000-4000-8000-000000000001\""),
                "{script}"
            );
        }
        let definition = azure
            .cli
            .split("cat > qucelerate-batches.json <<'EOF'\n")
            .nth(1)
            .and_then(|rest| rest.split("\nEOF").next())
            .unwrap();
        let definition: serde_json::Value = serde_json::from_str(definition).unwrap();
        assert_eq!(
            definition["DataActions"],
            serde_json::json!(AZURE_BATCH_DATA_ACTIONS)
        );
        // Named for its subscription: names are unique in the directory.
        let role = format!("{AZURE_BATCH_ROLE} 00000000-0000-4000-8000-000000000001");
        assert_eq!(definition["Name"], serde_json::json!(role));
        assert!(
            azure
                .terraform
                .contains(&format!("name        = \"{role}\""))
        );
        assert!(
            azure
                .cli
                .contains(&format!("--role \"{role}\" \\\n  --scope \"{resource}\""))
        );
        let placeholder = identities().azure(org, None, false, true).unwrap();
        assert!(placeholder.cli.contains("/subscriptions/<subscription>"));
        shell_parses(&placeholder.cli);
        assert!(matches!(
            identities().azure(org, Some(resource), true, true),
            Err(OnboardingError::AzureBatch)
        ));
    }

    #[test]
    fn google_batches_grant_the_bucket_to_the_account_and_the_service_agent() {
        let plain = identities().google("acme-ai-123", None).unwrap();
        assert_eq!(plain.batch_bucket, None);
        assert!(!plain.gcloud.contains("storage"));
        assert!(!plain.terraform.contains("storage"));
        let google = identities()
            .google("acme-ai-123", Some("acme-batches"))
            .unwrap();
        assert_eq!(google.batch_bucket.as_deref(), Some("acme-batches"));
        shell_parses(&google.gcloud);
        terraform_parses(&google.terraform);
        assert!(google.gcloud.ends_with("fi\n"));
        for script in [&google.gcloud, &google.terraform] {
            assert!(script.contains("roles/storage.objectCreator"), "{script}");
            assert!(script.contains("roles/storage.objectViewer"), "{script}");
        }
        assert!(google.gcloud.contains("gs://acme-batches"));
        assert!(
            google.gcloud.contains(
                "serviceAccount:service-$number@gcp-sa-aiplatform.iam.gserviceaccount.com"
            )
        );
        assert!(google.terraform.contains("bucket   = \"acme-batches\""));
        assert!(google.terraform.contains("google_project_service_identity"));
        for invalid in [
            "Acme",
            "ab",
            "-acme",
            "acme batches",
            "acme;rm",
            "gs://acme",
        ] {
            assert!(
                matches!(
                    identities().google("acme-ai-123", Some(invalid)),
                    Err(OnboardingError::GoogleBatch)
                ),
                "{invalid}"
            );
        }
    }
}
