{{- define "artifact-kit.fullname" -}}
{{- default .Release.Name .Values.fullnameOverride | trunc 50 | trimSuffix "-" -}}
{{- end -}}

{{- define "artifact-kit.labels" -}}
app.kubernetes.io/name: artifact-kit
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | quote }}
{{- end -}}

{{- define "artifact-kit.selector" -}}
app.kubernetes.io/name: artifact-kit
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "artifact-kit.credentials" -}}
{{- default (printf "%s-credentials" (include "artifact-kit.fullname" .)) .Values.secrets.credentials -}}
{{- end -}}

{{- define "artifact-kit.pki" -}}
{{- default (printf "%s-pki" (include "artifact-kit.fullname" .)) .Values.secrets.pki -}}
{{- end -}}

{{- define "artifact-kit.s3Endpoint" -}}
{{- if .Values.s3.endpoint -}}
{{- .Values.s3.endpoint -}}
{{- else if .Values.development.insecureHttp -}}
{{- printf "http://%s-s3:8333" (include "artifact-kit.fullname" .) -}}
{{- else -}}
{{- .Values.s3.publicEndpoint -}}
{{- end -}}
{{- end -}}

{{- define "artifact-kit.containerSecurity" -}}
allowPrivilegeEscalation: false
capabilities:
  drop: [ALL]
readOnlyRootFilesystem: true
{{- end -}}
