{{- define "gateway.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* The gateway image's tag: image.tag, else the chart's appVersion (the
     release the chart was published with). */}}
{{- define "gateway.imageTag" -}}
{{- .Values.image.tag | default .Chart.AppVersion -}}
{{- end -}}

{{/* The service account the API, worker and data plane run as. */}}
{{- define "gateway.serviceAccountName" -}}
{{- .Values.cloudIdentity.serviceAccount.name | default (include "gateway.name" .) -}}
{{- end -}}

{{- define "gateway.labels" -}}
app.kubernetes.io/name: {{ include "gateway.name" . }}
app.kubernetes.io/part-of: ai-gateway
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version | replace "+" "_" }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "gateway.selectorLabels" -}}
app.kubernetes.io/name: {{ include "gateway.name" .root }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{/* The gateway's environment: settings from values, origin-derived ones,
     and what each instance reports about itself (Organization -> Routing).
     A data plane serves no admin API or sign-in, so it gets no origin and
     none of the OIDC or onboarding settings. */}}
{{- define "gateway.env" -}}
{{- $dataPlane := eq .profile "data-plane" -}}
- name: GATEWAY_PROFILE
  value: {{ .profile | quote }}
- name: BIND_ADDR
  value: {{ printf "%s:3000" .root.Values.network.listenAddress | quote }}
- name: GATEWAY_BUILD
  value: {{ include "gateway.imageTag" .root | quote }}
{{- with ternary (.root.Values.dataPlane.region | default .root.Values.region) .root.Values.region $dataPlane }}
- name: GATEWAY_REGION
  value: {{ . | quote }}
{{- end }}
{{- if not $dataPlane }}
- name: GATEWAY_ORIGIN
  value: {{ printf "https://%s" (required "host is required: the browser-visible host of the gateway" .root.Values.host) | quote }}
- name: OIDC_REDIRECT_URI
  value: {{ printf "https://%s/api/admin/v1/oidc/callback" (required "host is required: the browser-visible host of the gateway" .root.Values.host) | quote }}
{{- end }}
{{- range $name, $value := .root.Values.env }}
{{- if not (and $dataPlane (or (hasPrefix "OIDC_" $name) (hasPrefix "GATEWAY_ONBOARDING_" $name))) }}
- name: {{ $name }}
  value: {{ $value | quote }}
{{- end }}
{{- end }}
{{- end -}}

{{/* No root filesystem writes, no privileges. */}}
{{- define "gateway.containerSecurity" -}}
allowPrivilegeEscalation: false
readOnlyRootFilesystem: true
capabilities:
  drop: ["ALL"]
{{- end -}}

{{- define "gateway.podSecurity" -}}
runAsNonRoot: true
seccompProfile:
  type: RuntimeDefault
{{- end -}}

{{/* The gateway image's user, distroless `nonroot`, by number: Kubernetes
     can only verify runAsNonRoot for a numeric user. */}}
{{- define "gateway.runAs" -}}
runAsUser: 65532
runAsGroup: 65532
{{- end -}}

{{/* Which clouds use a federated token. */}}
{{- define "gateway.federated" -}}
{{- $c := .Values.cloudIdentity -}}
{{- if or $c.aws.roleArn $c.azure.clientId $c.google.workloadIdentityProvider }}true{{ end -}}
{{- end -}}

{{/* Pod spec lines for the API, worker and data plane: the service account,
     the cloud-identity volumes, and the TLS secret `tlsSecret` names (for
     the QUIC listener; "" for none). Called with (dict "root" . "tlsSecret" ...). */}}
{{- define "gateway.cloudPod" -}}
{{- $root := .root -}}
{{- $c := $root.Values.cloudIdentity -}}
{{- $federated := include "gateway.federated" $root -}}
serviceAccountName: {{ include "gateway.serviceAccountName" $root }}
{{- if or $federated .tlsSecret }}
volumes:
{{- if $federated }}
  - name: cloud-tokens
    projected:
      sources:
        {{- if $c.aws.roleArn }}
        - serviceAccountToken:
            audience: sts.amazonaws.com
            expirationSeconds: 3600
            path: aws
        {{- end }}
        {{- if $c.azure.clientId }}
        - serviceAccountToken:
            audience: api://AzureADTokenExchange
            expirationSeconds: 3600
            path: azure
        {{- end }}
        {{- if $c.google.workloadIdentityProvider }}
        - serviceAccountToken:
            audience: {{ printf "https:%s" $c.google.workloadIdentityProvider | quote }}
            expirationSeconds: 3600
            path: google
        - configMap:
            name: {{ include "gateway.name" $root }}-google-credentials
            items:
              - key: credentials.json
                path: google-credentials.json
        {{- end }}
{{- end }}
{{- with .tlsSecret }}
  # The certificate cert-manager keeps here; renewals reach the mounted
  # files, which the gateway reads again.
  - name: tls
    secret:
      secretName: {{ . }}
      items:
        - {key: tls.crt, path: tls.crt}
        - {key: tls.key, path: tls.key}
{{- end }}
{{- end }}
{{- end -}}

{{/* Container lines for the API, worker and data plane: the mounts of the
     volumes above. Called like gateway.cloudPod. */}}
{{- define "gateway.cloudContainer" -}}
{{- $federated := include "gateway.federated" .root -}}
{{- if or $federated .tlsSecret }}
volumeMounts:
{{- if $federated }}
  - name: cloud-tokens
    mountPath: /var/run/secrets/cloud
    readOnly: true
{{- end }}
{{- if .tlsSecret }}
  - name: tls
    mountPath: /etc/gateway/tls
    readOnly: true
{{- end }}
{{- end }}
{{- end -}}

{{- define "gateway.cloudEnv" -}}
{{- $c := .Values.cloudIdentity -}}
{{- if $c.aws.roleArn }}
- name: AWS_ROLE_ARN
  value: {{ $c.aws.roleArn | quote }}
- name: AWS_WEB_IDENTITY_TOKEN_FILE
  value: /var/run/secrets/cloud/aws
{{- end }}
{{- if $c.azure.clientId }}
- name: AZURE_TENANT_ID
  value: {{ $c.azure.tenantId | quote }}
- name: AZURE_CLIENT_ID
  value: {{ $c.azure.clientId | quote }}
- name: AZURE_FEDERATED_TOKEN_FILE
  value: /var/run/secrets/cloud/azure
{{- end }}
{{- if $c.google.workloadIdentityProvider }}
- name: GOOGLE_APPLICATION_CREDENTIALS
  value: /var/run/secrets/cloud/google-credentials.json
{{- end }}
{{- end -}}

{{/* The metrics listener, on the API, worker and data plane (not the
     migration Job, which serves nothing). */}}
{{- define "gateway.metricsEnv" -}}
{{- if .Values.metrics.enabled }}
- name: GATEWAY_METRICS_ADDR
  value: {{ printf "%s:%v" .Values.network.listenAddress .Values.metrics.port | quote }}
{{- end }}
{{- end -}}

{{- define "gateway.metricsPort" -}}
{{- if .Values.metrics.enabled }}
- name: metrics
  containerPort: {{ .Values.metrics.port }}
{{- end }}
{{- end -}}

{{- define "gateway.metricsAnnotations" -}}
{{- if and .Values.metrics.enabled .Values.metrics.podAnnotations }}
annotations:
  prometheus.io/scrape: "true"
  prometheus.io/port: {{ .Values.metrics.port | quote }}
  prometheus.io/path: /metrics
{{- end }}
{{- end -}}

{{/* A Service's address families, when values set them (IPv6). */}}
{{- define "gateway.serviceFamilies" -}}
{{- with .Values.network.ipFamilyPolicy }}
ipFamilyPolicy: {{ . }}
{{- end }}
{{- with .Values.network.ipFamilies }}
ipFamilies: {{- toYaml . | nindent 2 }}
{{- end }}
{{- end -}}

{{/* HTTP/3: the gateway's own QUIC listener, on the API and data plane
     pods, presenting the TLS secret of the host it serves. Needs a gateway
     image with GATEWAY_HTTP3_ADDR (D-027). */}}
{{- define "gateway.http3Env" -}}
{{- if .root.Values.http3.enabled }}
- name: GATEWAY_HTTP3_ADDR
  value: {{ printf "%s:%v" .root.Values.network.listenAddress .root.Values.http3.containerPort | quote }}
- name: GATEWAY_TLS_CERT_FILE
  value: /etc/gateway/tls/tls.crt
- name: GATEWAY_TLS_KEY_FILE
  value: /etc/gateway/tls/tls.key
- name: GATEWAY_ALT_SVC
  value: {{ .altSvc | default "off" | quote }}
{{- end }}
{{- end -}}

{{- define "gateway.http3Port" -}}
{{- if .Values.http3.enabled }}
- name: h3
  containerPort: {{ .Values.http3.containerPort }}
  protocol: UDP
{{- end }}
{{- end -}}

{{/* The Service that publishes a component's QUIC listener on UDP. */}}
{{- define "gateway.http3Service" -}}
{{- $h3 := .root.Values.http3 -}}
apiVersion: v1
kind: Service
metadata:
  name: {{ include "gateway.name" .root }}-{{ .component }}-h3
  labels:
    {{- include "gateway.labels" .root | nindent 4 }}
    app.kubernetes.io/component: {{ .component }}
  {{- with $h3.service.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  type: {{ $h3.service.type }}
  {{- if eq $h3.service.type "LoadBalancer" }}
  externalTrafficPolicy: {{ $h3.service.externalTrafficPolicy }}
  {{- end }}
  {{- include "gateway.serviceFamilies" .root | nindent 2 }}
  selector:
    {{- include "gateway.selectorLabels" (dict "root" .root "component" .component) | nindent 4 }}
  ports:
    - name: h3
      port: {{ $h3.port }}
      targetPort: h3
      protocol: UDP
{{- end -}}
