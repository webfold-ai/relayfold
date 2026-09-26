{{/*
Standard chart name/labels helpers — same shape every real Helm chart uses (`helm create`'s own
starter chart has the identical set), not invented here.
*/}}
{{- define "relayfold-proxy.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "relayfold-proxy.fullname" -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "relayfold-proxy.labels" -}}
app.kubernetes.io/name: {{ include "relayfold-proxy.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "relayfold-proxy.selectorLabels" -}}
app.kubernetes.io/name: {{ include "relayfold-proxy.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
