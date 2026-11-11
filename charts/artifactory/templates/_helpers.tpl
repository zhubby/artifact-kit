{{- define "artifactory.fullname" -}}
{{- default .Release.Name .Values.fullnameOverride | trunc 50 | trimSuffix "-" -}}
{{- end -}}

{{- define "artifactory.labels" -}}
app.kubernetes.io/name: artifactory
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | quote }}
{{- end -}}

{{- define "artifactory.selector" -}}
app.kubernetes.io/name: artifactory
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "artifactory.credentials" -}}
{{- default (printf "%s-credentials" (include "artifactory.fullname" .)) .Values.secrets.credentials -}}
{{- end -}}

{{- define "artifactory.pki" -}}
{{- default (printf "%s-pki" (include "artifactory.fullname" .)) .Values.secrets.pki -}}
{{- end -}}

{{- define "artifactory.s3Endpoint" -}}
{{- if .Values.s3.endpoint -}}
{{- .Values.s3.endpoint -}}
{{- else if .Values.development.insecureHttp -}}
{{- printf "http://%s-s3:8333" (include "artifactory.fullname" .) -}}
{{- else -}}
{{- .Values.s3.publicEndpoint -}}
{{- end -}}
{{- end -}}

{{- define "artifactory.containerSecurity" -}}
allowPrivilegeEscalation: false
capabilities:
  drop: [ALL]
readOnlyRootFilesystem: true
{{- end -}}
