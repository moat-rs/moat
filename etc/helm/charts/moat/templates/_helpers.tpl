{{/*
Expand the name of the chart.
*/}}
{{- define "moat.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "moat.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Cache component name
*/}}
{{- define "moat.cache.fullname" -}}
{{- printf "%s-cache" (include "moat.fullname" .) }}
{{- end }}

{{/*
Agent component name
*/}}
{{- define "moat.agent.fullname" -}}
{{- printf "%s-agent" (include "moat.fullname" .) }}
{{- end }}


{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "moat.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "moat.labels" -}}
helm.sh/chart: {{ include "moat.chart" . }}
{{ include "moat.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "moat.selectorLabels" -}}
app.kubernetes.io/name: {{ include "moat.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Cache selector labels
*/}}
{{- define "moat.cache.selectorLabels" -}}
{{ include "moat.selectorLabels" . }}
app.kubernetes.io/component: cache
{{- end }}

{{/*
Agent selector labels
*/}}
{{- define "moat.agent.selectorLabels" -}}
{{ include "moat.selectorLabels" . }}
app.kubernetes.io/component: agent
{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "moat.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "moat.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Generate the bootstrap peers for moat.
*/}}
{{- define "moat.bootstrapPeers" -}}
{{- $peers := .Values.moat.bootstrapPeers -}}
{{- if not $peers -}}
  {{- $replicaCount := .Values.moat.replica.cache | int -}}
  {{- $name := include "moat.name" . -}}
  {{- $peerList := list -}}
  {{- range $i := until $replicaCount -}}
    {{- $peer := printf "%s-%d.%s:%d" $name $i $name 23456 -}}
    {{- $peerList = append $peerList $peer -}}
  {{- end -}}
  {{- $peers = join "," $peerList -}}
{{- end -}}
{{- $peers -}}
{{- end }}
