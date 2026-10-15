{{/*
Image reference: repository[:tag]@digest when there is a digest (the tag is then informational), repository:tag otherwise.
Refuses to render with neither.
*/}}
{{- define "cloudhv-installer.image" -}}
{{- $i := .image -}}
{{- if not (or $i.digest $i.tag) -}}
{{- fail "the installer image needs a digest (image.digest, preferred) or a tag (image.tag): there is no default tag" -}}
{{- end -}}
{{- if $i.digest -}}
{{- if $i.tag -}}{{ $i.repository }}:{{ $i.tag }}@{{ $i.digest }}{{- else -}}{{ $i.repository }}@{{ $i.digest }}{{- end -}}
{{- else -}}
{{ $i.repository }}:{{ $i.tag }}
{{- end -}}
{{- end -}}

{{/*
Pull policy: the value if set; otherwise IfNotPresent for an image given by digest (it cannot change), Always for a tag.
*/}}
{{- define "cloudhv-installer.pullPolicy" -}}
{{- if .pullPolicy -}}{{ .pullPolicy }}{{- else if .digest -}}IfNotPresent{{- else -}}Always{{- end -}}
{{- end -}}
