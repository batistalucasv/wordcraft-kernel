//! Ready-made R and Python snippets for ecology and statistics teaching.
//!
//! Every snippet is a complete, commented script that runs on synthetic data,
//! so it can be inserted into a document and executed immediately. Data,
//! titles, seeds and confidence levels are exposed as [`TemplateParam`]s and
//! replaced by [`Template::render`].
//!
//! Placeholders are written `{{key}}` and are substituted verbatim, because the
//! value often is source code itself (an R expression, a column name, a path).
//! Callers that accept values from an untrusted source must validate or quote
//! them for the target language before rendering.
//!
//! ```no_run
//! use wordcraft_kernel::templates;
//!
//! let template = templates::find("r.shannon-simpson").expect("built-in template");
//! let script = template.render(&[("title", "Diversity at three sites")])?;
//! let request = wordcraft_kernel::ExecutionRequest::new(template.language, script);
//! # Ok::<(), wordcraft_kernel::KernelError>(())
//! ```

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{CodeLanguage, ExecutionRequest, KernelError};

/// Topic used to group templates in the snippet browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateCategory {
    /// Alpha diversity indices for a community matrix.
    Diversity,
    /// Species accumulation curves and rarefaction.
    Accumulation,
    /// Linear models with confidence bands.
    Regression,
    /// t-tests, analysis of variance and post-hoc comparisons.
    HypothesisTesting,
    /// Reading, cleaning and summarising tabular data.
    DataIngestion,
    /// Correlation matrices and heat maps.
    Correlation,
}

impl TemplateCategory {
    /// Every category, in the order the library presents them.
    pub const ALL: [TemplateCategory; 6] = [
        TemplateCategory::Diversity,
        TemplateCategory::Accumulation,
        TemplateCategory::Regression,
        TemplateCategory::HypothesisTesting,
        TemplateCategory::DataIngestion,
        TemplateCategory::Correlation,
    ];

    /// Stable identifier used in filters and serialized data.
    pub fn as_str(self) -> &'static str {
        match self {
            TemplateCategory::Diversity => "diversity",
            TemplateCategory::Accumulation => "accumulation",
            TemplateCategory::Regression => "regression",
            TemplateCategory::HypothesisTesting => "hypothesis_testing",
            TemplateCategory::DataIngestion => "data_ingestion",
            TemplateCategory::Correlation => "correlation",
        }
    }

    /// Human-readable name for menus and sidebars.
    pub fn label(self) -> &'static str {
        match self {
            TemplateCategory::Diversity => "Diversity indices",
            TemplateCategory::Accumulation => "Species accumulation",
            TemplateCategory::Regression => "Regression",
            TemplateCategory::HypothesisTesting => "Hypothesis testing",
            TemplateCategory::DataIngestion => "Data ingestion",
            TemplateCategory::Correlation => "Correlation",
        }
    }
}

impl fmt::Display for TemplateCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A substitutable value in a template's source.
///
/// Every parameter has a default, so a template always renders without input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TemplateParam {
    /// Placeholder key, written as `{{key}}` in the snippet.
    pub key: &'static str,
    /// Human-readable name for an input field.
    pub label: &'static str,
    /// Value used when the caller does not override it.
    pub default: &'static str,
    /// One sentence explaining what the value changes.
    pub description: &'static str,
}

/// A built-in snippet: metadata, declared dependencies and source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Template {
    /// Stable identifier (`"r.shannon-simpson"`) used by commands and documents.
    pub id: &'static str,
    /// Short name for a menu entry.
    pub title: &'static str,
    /// One sentence describing what the snippet demonstrates.
    pub description: &'static str,
    /// Language the snippet is written in.
    pub language: CodeLanguage,
    /// Topic used to group snippets.
    pub category: TemplateCategory,
    /// Third-party packages the snippet needs, including packages used only by a
    /// documented option (for example scipy for Kendall correlation).
    ///
    /// Tests run the snippet only when every listed package is installed.
    pub requires: &'static [&'static str],
    /// Substitutable values, all with defaults.
    pub params: &'static [TemplateParam],
    /// Snippet source with `{{key}}` placeholders.
    pub code: &'static str,
}

impl Template {
    /// Render the snippet with every parameter at its default value.
    pub fn render_default(&self) -> Result<String, KernelError> {
        self.render(&[])
    }

    /// Render the snippet, replacing each `{{key}}` with its value.
    ///
    /// Values come from `overrides` first and from the parameter defaults
    /// afterwards. An override for a key the template does not declare is an
    /// error rather than a silent no-op, so a typo cannot produce a script that
    /// ignores what the caller asked for; when a key is repeated, the first
    /// value wins. Values are inserted verbatim and are never re-scanned, so a
    /// value containing `{{...}}` stays literal.
    pub fn render(&self, overrides: &[(&str, &str)]) -> Result<String, KernelError> {
        for (key, _) in overrides {
            if self.param(key).is_none() {
                return Err(KernelError::UnknownTemplateParameter { template: self.id.to_string(), parameter: (*key).to_string() });
            }
        }

        let mut rendered = String::with_capacity(self.code.len());
        let mut rest = self.code;
        loop {
            let Some(start) = rest.find("{{") else {
                rendered.push_str(rest);
                return Ok(rendered);
            };
            rendered.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                return Err(KernelError::UnterminatedPlaceholder { template: self.id.to_string() });
            };
            let key = after[..end].trim();
            let value = overrides
                .iter()
                .find(|(candidate, _)| *candidate == key)
                .map(|(_, value)| *value)
                .or_else(|| self.param(key).map(|param| param.default))
                .ok_or_else(|| KernelError::UndefinedPlaceholder { template: self.id.to_string(), placeholder: key.to_string() })?;
            rendered.push_str(value);
            rest = &after[end + 2..];
        }
    }

    /// Build an execution request for the snippet rendered with its defaults.
    pub fn execution_request(&self) -> Result<ExecutionRequest, KernelError> {
        Ok(ExecutionRequest::new(self.language, self.render_default()?))
    }

    /// Look up a declared parameter by key.
    fn param(&self, key: &str) -> Option<&'static TemplateParam> {
        self.params.iter().find(|param| param.key == key)
    }
}

/// Every built-in template, R before Python.
pub fn all() -> &'static [Template] {
    TEMPLATES
}

/// Find a template by id, ignoring ASCII case.
pub fn find(id: &str) -> Option<&'static Template> {
    all().iter().find(|template| template.id.eq_ignore_ascii_case(id))
}

/// Iterate over the templates written in one language.
pub fn by_language(language: CodeLanguage) -> impl Iterator<Item = &'static Template> {
    all().iter().filter(move |template| template.language == language)
}

/// Iterate over the templates in one category.
pub fn by_category(category: TemplateCategory) -> impl Iterator<Item = &'static Template> {
    all().iter().filter(move |template| template.category == category)
}

/// Every category, for building a filtered browser.
pub fn categories() -> &'static [TemplateCategory] {
    &TemplateCategory::ALL
}

const COMMUNITY_DEFAULT: &str = r#"data.frame(
  sp1 = c(12, 4, 0),
  sp2 = c(7, 9, 1),
  sp3 = c(3, 11, 20),
  sp4 = c(0, 2, 16),
  sp5 = c(5, 8, 4),
  row.names = c("site_a", "site_b", "site_c")
)"#;

const R_SHANNON_SIMPSON: &str = r##"# Alpha diversity: Shannon-Wiener and Simpson indices per site.
# Requires: vegan. Input is a site x species abundance table (rows are sites).
suppressPackageStartupMessages(library(vegan))

# Replace the default table with your own, for example:
#   read.csv("my_abundance.csv", row.names = 1)
comm <- {{community}}

# Shannon-Wiener H' = -sum(p_i * ln p_i) and Simpson 1 - D per site.
shannon <- diversity(comm, index = "shannon")
simpson <- diversity(comm, index = "simpson")

# Richness (S) and Pielou evenness J' = H' / ln(S) complete the profile.
richness <- specnumber(comm)
evenness <- ifelse(richness > 1, shannon / log(richness), NA_real_)

results <- data.frame(
  site = rownames(comm),
  richness = richness,
  shannon = round(shannon, 3),
  simpson = round(simpson, 3),
  evenness = round(evenness, 3),
  row.names = NULL
)
print(results)

# Bar chart of H', captured by WordCraft as the chunk's plot.
barplot(
  results$shannon,
  names.arg = results$site,
  main = "{{title}}",
  ylab = "Shannon-Wiener H'",
  col = "#4E79A7",
  border = NA
)
"##;

const R_SPECIES_ACCUMULATION: &str = r##"# Species accumulation and rarefaction with vegan.
# Requires: vegan. The default matrix is synthetic presence/absence data.
suppressPackageStartupMessages(library(vegan))

set.seed(2024)
n_units <- {{n_units}}
n_species <- {{n_species}}

# Species differ in detectability, so the curve is still rising at the last unit.
detectability <- seq(0.75, 0.15, length.out = n_species)
community <- sapply(
  detectability,
  function(p) rbinom(n_units, size = 1, prob = p)
)
rownames(community) <- sprintf("unit_%02d", seq_len(n_units))
colnames(community) <- sprintf("sp_%02d", seq_len(n_species))

# Randomised (permutation-based) species accumulation with a 95% confidence
# envelope: each point is the mean over `permutations` orderings of the units.
accumulation <- specaccum(
  community,
  method = "random",
  permutations = {{permutations}}
)
print(accumulation)

# Hurlbert's rarefaction curve: expected richness in a subsample of each size,
# which compares units at equal sampling effort. Interpolation only.
rarefaction <- specaccum(community, method = "rarefaction")
print(rarefaction)

# Chao's estimator of the asymptotic species pool.
pool <- specpool(community)
print(pool[, c("Species", "chao", "chao.se")])

plot(
  accumulation,
  main = "{{title}}",
  xlab = "Sampling units",
  ylab = "Species",
  col = "#4E79A7",
  ci.type = "poly",
  ci.col = adjustcolor("#4E79A7", alpha.f = 0.25)
)
lines(rarefaction, col = "#E15759", lty = 2)
legend(
  "bottomright",
  legend = c("Accumulation (randomised)", "Rarefaction (Hurlbert)"),
  col = c("#4E79A7", "#E15759"),
  lty = c(1, 2),
  bty = "n"
)
"##;

const R_LM_CONFIDENCE: &str = r##"# Linear regression with a confidence band, drawn with ggplot2.
# Requires: ggplot2. One row per observation; replace `survey` with your data.
suppressPackageStartupMessages(library(ggplot2))

set.seed({{seed}})
n <- {{n_observations}}

# Synthetic survey: species richness driven by temperature and elevation.
survey <- data.frame(
  temperature = runif(n, 8, 30),
  elevation = runif(n, 0, 1200)
)
survey$richness <- round(
  50 - 0.6 * survey$temperature - 0.01 * survey$elevation + rnorm(n, 0, 4)
)

# Additive model: richness ~ temperature + elevation.
fit <- lm(richness ~ temperature + elevation, data = survey)
print(summary(fit))

# Confidence band for temperature while elevation is held at its mean.
grid <- data.frame(
  temperature = seq(
    min(survey$temperature),
    max(survey$temperature),
    length.out = 100
  ),
  elevation = mean(survey$elevation)
)
grid <- cbind(
  grid,
  predict(fit, newdata = grid, interval = "confidence", level = {{confidence}})
)

# Print explicitly: inside tryCatch() R does not auto-print plot objects.
print(
  ggplot(grid, aes(x = temperature, y = fit)) +
    geom_ribbon(aes(ymin = lwr, ymax = upr), fill = "#4E79A7", alpha = 0.25) +
    geom_line(colour = "#4E79A7", linewidth = 1) +
    geom_point(
      data = survey,
      aes(x = temperature, y = richness),
      inherit.aes = FALSE,
      alpha = 0.5
    ) +
    labs(
      title = "{{title}}",
      subtitle = sprintf(
        "Linear model with %d%% confidence band",
        round({{confidence}} * 100)
      ),
      x = "Temperature (C)",
      y = "Species richness"
    ) +
    theme_minimal()
)
"##;

const R_TTEST_ANOVA: &str = r##"# Student's t-test and one-way ANOVA with box plots (ggplot2).
# Requires: ggplot2. One row per measured sample; replace `biomass` with yours.
suppressPackageStartupMessages(library(ggplot2))

set.seed({{seed}})
n_per_group <- {{n_per_group}}

# Synthetic biomass, three habitats with different means.
biomass <- data.frame(
  habitat = factor(rep(c("Forest", "Grassland", "Wetland"), each = n_per_group)),
  biomass = c(
    rnorm(n_per_group, mean = 32, sd = 5),
    rnorm(n_per_group, mean = 24, sd = 4),
    rnorm(n_per_group, mean = 20, sd = 6)
  )
)

# Two-sample Student's t-test (pooled variance): Forest vs. Grassland.
pair <- subset(biomass, habitat %in% c("Forest", "Grassland"))
print(t.test(biomass ~ habitat, data = pair, var.equal = TRUE))

# One-way ANOVA across all three habitats, followed by Tukey's HSD.
model <- aov(biomass ~ habitat, data = biomass)
print(summary(model))
print(TukeyHSD(model))

# Box plots with the raw observations overlaid.
# Print explicitly: inside tryCatch() R does not auto-print plot objects.
print(
  ggplot(biomass, aes(x = habitat, y = biomass, fill = habitat)) +
    geom_boxplot(width = 0.55, outlier.shape = NA) +
    geom_jitter(width = 0.12, alpha = 0.6, size = 1.8) +
    labs(
      title = "{{title}}",
      subtitle = sprintf("One-way ANOVA p = %.4f", summary(model)[[1]][["Pr(>F)"]][1]),
      x = NULL,
      y = "Biomass (g/m2)"
    ) +
    theme_minimal() +
    theme(legend.position = "none")
)
"##;

const PYTHON_DATA_INGEST: &str = r##"# Data ingestion and summary with pandas.
# Requires: pandas. Replace the synthetic block with your own survey table.
import pandas as pd

# A tiny synthetic survey, written to CSV first so the snippet exercises the
# same path real data takes.
raw = pd.DataFrame({
    "plot": ["A1", "A2", "B1", "B2", "C1", "C2"],
    "habitat": ["forest", "forest", "grassland", "grassland", "wetland", "wetland"],
    "individuals": [42, 38, 25, 31, 18, 22],
    "richness": [11, 13, 8, 9, 6, 7],
    "soil_ph": [5.8, 5.6, 6.7, 6.9, 7.4, 7.2],
})

csv_path = "{{csv_path}}"
raw.to_csv(csv_path, index=False)

# 1. Ingest: parse the CSV, then drop rows with missing measurements.
data = pd.read_csv(csv_path).dropna(subset=["individuals", "richness", "soil_ph"])

# 2. Inspect shape and dtypes before trusting the numbers.
print(f"{data.shape[0]} rows x {data.shape[1]} columns\n")
print(data.dtypes, "\n")

# 3. Numeric summary: count, mean, standard deviation and quartiles.
print(data.describe().round(2))

# 4. Summarise per group, the way a lab report needs it.
summary = (
    data.groupby("{{group_column}}")[["individuals", "richness", "soil_ph"]]
    .agg(["mean", "std", "min", "max"])
    .round(2)
)
print("\nPer-group summary:")
print(summary)

# 5. Pearson correlations between the numeric columns.
print("\nCorrelation matrix:")
print(data.select_dtypes("number").corr().round(3))
"##;

const PYTHON_CORRELATION_HEATMAP: &str = r##"# Correlation matrix and heat map with pandas and seaborn.
# Requires: pandas, seaborn, matplotlib. Replace the synthetic block with yours.
import matplotlib.pyplot as plt
import numpy as np
import pandas as pd
import seaborn as sns

rng = np.random.default_rng({{seed}})
n = {{n_samples}}

# Synthetic environmental measurements, one row per sample.
temperature = rng.normal(18, 5, n)
humidity = 90 - 2.5 * temperature + rng.normal(0, 8, n)
richness = 40 - 0.8 * temperature + 0.15 * humidity + rng.normal(0, 3, n)
soil_ph = rng.normal(6.4, 0.6, n)

data = pd.DataFrame({
    "temperature": temperature,
    "humidity": humidity,
    "richness": richness,
    "soil_ph": soil_ph,
})

# Correlation matrix; use "spearman" for a rank-based measure.
corr = data.corr(method="{{method}}")
print(corr.round(3))

# Heat map on a diverging scale centred on zero, so the sign is obvious.
fig, ax = plt.subplots(figsize=(7, 5))
sns.heatmap(
    corr,
    annot=True,
    fmt=".2f",
    cmap="RdBu_r",
    vmin=-1,
    vmax=1,
    square=True,
    linewidths=0.5,
    cbar_kws={"label": "correlation"},
    ax=ax,
)
ax.set_title("{{title}}")
fig.tight_layout()
"##;

const PYTHON_LM_TESTS: &str = r##"# Linear regression and hypothesis tests with scipy.stats.
# Requires: numpy, pandas, scipy. Replace the synthetic block with your data.
import numpy as np
import pandas as pd
from scipy import stats

rng = np.random.default_rng({{seed}})
n = {{n_samples}}
confidence = {{confidence}}

# One row per observation: a temperature reading and an abundance count.
temperature = rng.uniform(5, 30, n)
abundance = 60 - 1.2 * temperature + rng.normal(0, 5, n)
band = np.where(temperature < 17.5, "low", "high")
data = pd.DataFrame({"temperature": temperature, "abundance": abundance, "band": band})

# Ordinary least squares: slope, intercept, r, p-value and standard error.
slope, intercept, r_value, p_value, std_err = stats.linregress(
    data["temperature"], data["abundance"]
)
print(f"slope = {slope:.3f} +/- {std_err:.3f} (1 s.e.)")
print(f"intercept = {intercept:.3f}")
print(f"r = {r_value:.3f}, r^2 = {r_value ** 2:.3f}, p = {p_value:.3g}")

# Welch's t-test between the two temperature bands.
low = data.loc[data["band"] == "low", "abundance"]
high = data.loc[data["band"] == "high", "abundance"]
t_stat, t_p = stats.ttest_ind(low, high, equal_var=False)
print(f"\nWelch t-test: t = {t_stat:.3f}, p = {t_p:.3g}")
print(f"mean difference (low - high) = {low.mean() - high.mean():.3f}")

# Spearman's rank correlation: robust to monotonic non-linearity.
rho, rho_p = stats.spearmanr(data["temperature"], data["abundance"])
print(f"Spearman rho = {rho:.3f}, p = {rho_p:.3g}")

# Confidence band for the mean response, from the residual variance.
residuals = data["abundance"] - (intercept + slope * data["temperature"])
residual_error = np.sqrt(np.sum(residuals ** 2) / (n - 2))
x_mean = data["temperature"].mean()
sxx = np.sum((data["temperature"] - x_mean) ** 2)
t_critical = stats.t.ppf((1 + confidence) / 2, df=n - 2)

grid = np.linspace(data["temperature"].min(), data["temperature"].max(), 3)
fitted = intercept + slope * grid
margin = t_critical * residual_error * np.sqrt(1 / n + (grid - x_mean) ** 2 / sxx)
print("\nConfidence band at three temperatures:")
for value, fit, half_width in zip(grid, fitted, margin):
    print(
        f"  T = {value:6.2f} -> {fit:7.2f}"
        f" [{fit - half_width:7.2f}, {fit + half_width:7.2f}]"
    )
"##;

static TEMPLATES: &[Template] = &[
    Template {
        id: "r.shannon-simpson",
        title: "Shannon-Wiener and Simpson diversity",
        description: "Alpha diversity per site, with richness and Pielou evenness.",
        language: CodeLanguage::R,
        category: TemplateCategory::Diversity,
        requires: &["vegan"],
        params: &[
            TemplateParam {
                key: "community",
                label: "Abundance table",
                default: COMMUNITY_DEFAULT,
                description: "R expression producing a site x species abundance table.",
            },
            TemplateParam {
                key: "title",
                label: "Plot title",
                default: "Shannon-Wiener diversity by site",
                description: "Title drawn above the diversity bar chart.",
            },
        ],
        code: R_SHANNON_SIMPSON,
    },
    Template {
        id: "r.species-accumulation",
        title: "Species accumulation and rarefaction",
        description: "Randomised species accumulation with a confidence envelope, plus Hurlbert's rarefaction curve.",
        language: CodeLanguage::R,
        category: TemplateCategory::Accumulation,
        requires: &["vegan"],
        params: &[
            TemplateParam {
                key: "n_units",
                label: "Sampling units",
                default: "25",
                description: "Number of sampling units in the synthetic matrix.",
            },
            TemplateParam { key: "n_species", label: "Species", default: "12", description: "Number of species in the synthetic matrix." },
            TemplateParam {
                key: "permutations",
                label: "Permutations",
                default: "100",
                description: "Randomisations behind the confidence envelope.",
            },
            TemplateParam {
                key: "title",
                label: "Plot title",
                default: "Species accumulation and rarefaction",
                description: "Title drawn above the accumulation curve.",
            },
        ],
        code: R_SPECIES_ACCUMULATION,
    },
    Template {
        id: "r.lm-confidence",
        title: "Linear regression with a confidence band",
        description: "Multiple regression printed, with a 95% band drawn by ggplot2.",
        language: CodeLanguage::R,
        category: TemplateCategory::Regression,
        requires: &["ggplot2"],
        params: &[
            TemplateParam { key: "seed", label: "Random seed", default: "7", description: "Makes the synthetic observations reproducible." },
            TemplateParam { key: "n_observations", label: "Observations", default: "40", description: "Number of synthetic observations." },
            TemplateParam {
                key: "confidence",
                label: "Confidence level",
                default: "0.95",
                description: "Confidence level for the band, between 0 and 1.",
            },
            TemplateParam {
                key: "title",
                label: "Plot title",
                default: "Species richness along a temperature gradient",
                description: "Title drawn above the fitted line.",
            },
        ],
        code: R_LM_CONFIDENCE,
    },
    Template {
        id: "r.ttest-anova",
        title: "t-test and ANOVA with box plots",
        description: "Student's t-test, one-way ANOVA, Tukey HSD and grouped box plots.",
        language: CodeLanguage::R,
        category: TemplateCategory::HypothesisTesting,
        requires: &["ggplot2"],
        params: &[
            TemplateParam { key: "seed", label: "Random seed", default: "11", description: "Makes the synthetic measurements reproducible." },
            TemplateParam {
                key: "n_per_group",
                label: "Samples per habitat",
                default: "15",
                description: "Number of synthetic samples in each habitat.",
            },
            TemplateParam { key: "title", label: "Plot title", default: "Biomass by habitat", description: "Title drawn above the box plots." },
        ],
        code: R_TTEST_ANOVA,
    },
    Template {
        id: "python.data-ingest",
        title: "Data ingestion and summary",
        description: "Read a CSV with pandas, inspect it and summarise it per group.",
        language: CodeLanguage::Python,
        category: TemplateCategory::DataIngestion,
        requires: &["pandas"],
        params: &[
            TemplateParam {
                key: "csv_path",
                label: "CSV path",
                default: "field_survey.csv",
                description: "File the snippet writes and reads back, in the chunk directory.",
            },
            TemplateParam {
                key: "group_column",
                label: "Grouping column",
                default: "habitat",
                description: "Column used to group the per-group summary.",
            },
        ],
        code: PYTHON_DATA_INGEST,
    },
    Template {
        id: "python.correlation-heatmap",
        title: "Correlation matrix and heat map",
        description: "Pearson correlations printed and drawn as a seaborn heat map.",
        language: CodeLanguage::Python,
        category: TemplateCategory::Correlation,
        requires: &["pandas", "seaborn", "matplotlib", "scipy"],
        params: &[
            TemplateParam { key: "seed", label: "Random seed", default: "2024", description: "Makes the synthetic measurements reproducible." },
            TemplateParam { key: "n_samples", label: "Samples", default: "60", description: "Number of synthetic samples." },
            TemplateParam {
                key: "method",
                label: "Correlation method",
                default: "pearson",
                description: "Correlation method passed to pandas: pearson, spearman or kendall (Kendall needs scipy).",
            },
            TemplateParam {
                key: "title",
                label: "Plot title",
                default: "Environmental correlates of species richness",
                description: "Title drawn above the heat map.",
            },
        ],
        code: PYTHON_CORRELATION_HEATMAP,
    },
    Template {
        id: "python.lm-tests",
        title: "Linear regression and hypothesis tests",
        description: "scipy.stats regression, Welch t-test, Spearman rho and a confidence band.",
        language: CodeLanguage::Python,
        category: TemplateCategory::Regression,
        requires: &["numpy", "pandas", "scipy"],
        params: &[
            TemplateParam { key: "seed", label: "Random seed", default: "7", description: "Makes the synthetic observations reproducible." },
            TemplateParam { key: "n_samples", label: "Samples", default: "45", description: "Number of synthetic observations." },
            TemplateParam {
                key: "confidence",
                label: "Confidence level",
                default: "0.95",
                description: "Confidence level for the band, between 0 and 1.",
            },
        ],
        code: PYTHON_LM_TESTS,
    },
];
