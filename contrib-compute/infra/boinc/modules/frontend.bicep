// Static hosting for the human-facing project pages (docs, volunteer instructions).
// The Free plan is enough. It does NOT serve the BOINC master URL: the client needs
// get_project_config.php, which is dynamic, so the master URL points at the Container App.
param name string
param location string = 'eastus2' // Static Web Apps is available in a limited set of regions.

resource site 'Microsoft.Web/staticSites@2023-12-01' = {
  name: 'usnm-${name}-web'
  location: location
  sku: { name: 'Free', tier: 'Free' }
  properties: {
    stagingEnvironmentPolicy: 'Disabled'
    allowConfigFileUpdates: true
  }
}

output defaultHostname string = site.properties.defaultHostname
